//! IMAP server.
//!
//! Per-connection state machine driven by `serve_connection` over any
//! `AsyncRead + AsyncWrite` (a `TcpStream` in production, a
//! `tokio::io::DuplexStream` in tests). The accept loop in [`serve`]
//! spawns one task per accepted socket and stops accepting when the
//! shared shutdown future fires.
//!
//! v0 scope: plaintext only, accept any credential. Currently
//! implemented: greeting, `CAPABILITY`, `NOOP`, `LOGOUT`, `LOGIN`,
//! `AUTHENTICATE` (PLAIN / XOAUTH2 / OAUTHBEARER), `ENABLE QRESYNC`,
//! `LIST`, `STATUS`, `SELECT` / `EXAMINE`, `UID SEARCH`, `UID FETCH`.
//! Everything else returns tagged `BAD`. See
//! `notes/ratatoskr-imap-surface.md` for what the client expects on
//! the wire.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use crate::fixture::{Address, Body, Email, Fixture, Mailbox, OWNER_RIGHTS, Role};

/// Greeting line emitted as soon as the connection is accepted, before
/// the client says anything. Per RFC 3501 sec 7.1, an `* OK` greeting
/// puts the connection in the Not Authenticated state.
pub const GREETING: &str = "* OK saehrimnir IMAP4rev1 ready\r\n";

/// Capabilities advertised in response to `CAPABILITY` and on every
/// `OK [CAPABILITY ...]` resp-text. Authenticated set; the
/// pre-auth set adds `LOGINDISABLED`-equivalents only if we ever grow
/// real auth, which v0 does not.
pub const CAPABILITIES: &str = "IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL";

/// Other-users namespace prefix (RFC 2342). Mailboxes another account
/// has shared with the authenticated one (via a fixture `[[acl]]`
/// grant) surface under `#user/<owner>/<path>`. Advertised by
/// `NAMESPACE`; recognised on `LIST` / `SELECT` / `MYRIGHTS` /
/// `GETACL`.
pub const SHARED_NAMESPACE_PREFIX: &str = "#user/";

/// Reserved sentinel password. v0 auth is opt-in accept-everything, so
/// a harness script cannot otherwise provoke an authentication
/// failure. When a `LOGIN` (or `AUTHENTICATE PLAIN`) presents THIS
/// exact password, the server replies with a tagged `NO
/// [AUTHENTICATIONFAILED]` completion instead of binding the account,
/// letting a ratatoskr harness script prove a bad-password account
/// verify surfaces an `AccountError`. Case-sensitive exact match, so
/// the many existing sync-harness scripts that log in with arbitrary
/// passwords keep succeeding - only this reserved literal is rejected.
pub const REJECT_AUTH_PASSWORD: &str = "saehrimnir-reject-auth";

/// True when a presented basic-auth password is the reserved
/// rejection sentinel (see `REJECT_AUTH_PASSWORD`).
pub(crate) fn is_reject_password(pass: &str) -> bool {
    pass == REJECT_AUTH_PASSWORD
}

/// Per-connection state machine, RFC 3501 sec 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    NotAuthenticated,
    Authenticated,
    Selected,
    Logout,
}

/// Run the accept loop until `shutdown` flips. Each accepted connection
/// runs in its own task; in-flight connections drop when their socket
/// is closed or when the shutdown signal interrupts a read.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    fixture: crate::shared::FixtureHandle,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    token_store: crate::oauth::TokenStore,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    push: crate::push::PushHub,
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
                        let fix = Arc::clone(&fixture);
                        let disp = dispatcher.clone();
                        let store = token_store.clone();
                        let log = request_log.clone();
                        let lat = latency.clone();
                        let push = push.clone();
                        let mut sd = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, fix, disp, store, log, lat, push, &mut sd).await {
                                eprintln!("saehrimnir: imap connection {peer}: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("saehrimnir: imap accept error: {e}");
                    }
                }
            }
        }
    }
}

/// Drive a single IMAP connection. `dispatcher` is `None` for
/// callback-free scenarios (TOML fixtures or Lua scenarios that
/// registered no `on()` handlers); when present, protocol commands
/// consult it before generating their default responses.
#[allow(clippy::too_many_arguments)]
pub async fn serve_connection<S>(
    stream: S,
    fixture: crate::shared::FixtureHandle,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    token_store: crate::oauth::TokenStore,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    push: crate::push::PushHub,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let primary_account_id = fixture
        .read()
        .expect("fixture lock poisoned")
        .primary_account()
        .id
        .clone();
    let mut conn = Conn {
        reader: BufReader::new(reader),
        writer,
        state: State::NotAuthenticated,
        fixture,
        dispatcher,
        token_store,
        request_log,
        latency,
        selected: None,
        selected_account: None,
        selected_rights: None,
        account_id: primary_account_id,
        connection_id: crate::connection_id::next(),
        push,
        qresync_enabled: false,
    };

    conn.write_line(GREETING.trim_end_matches("\r\n")).await?;

    loop {
        let line = match conn.read_command_line(shutdown).await? {
            ReadOutcome::Line(l) => l,
            ReadOutcome::PeerClosed => return Ok(()),
            ReadOutcome::Shutdown => {
                let _ = conn.write_line("* BYE saehrimnir shutting down").await;
                return Ok(());
            }
        };
        conn.latency.sleep_for("imap").await;
        conn.dispatch(&line, shutdown).await?;
        if conn.state == State::Logout {
            return Ok(());
        }
    }
}

/// Per-connection bag. Reader/writer are split halves so a handler can
/// emit continuation prompts and read responses without re-locking.
struct Conn<S: AsyncRead + AsyncWrite + Unpin> {
    reader: BufReader<ReadHalf<S>>,
    writer: WriteHalf<S>,
    state: State,
    fixture: crate::shared::FixtureHandle,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    /// Token store. Used to resolve `AUTHENTICATE XOAUTH2` /
    /// `OAUTHBEARER` bearer tokens to the account they authorize,
    /// matching the OAuth flow Gmail / gcal / People use.
    token_store: crate::oauth::TokenStore,
    /// Cross-protocol request log handle. Cheap to clone (it's an
    /// `Arc<Mutex<...>>`); tests that don't care just pass
    /// `RequestLog::default()`.
    request_log: crate::request_log::RequestLog,
    /// Per-protocol latency knob (test-only). Consulted before each
    /// dispatched command so harness scripts can simulate slow links.
    latency: crate::latency::LatencyKnob,
    /// Fixture id of the currently selected mailbox, if any. Set by
    /// SELECT/EXAMINE, cleared on CLOSE/UNSELECT (which we don't yet
    /// handle).
    selected: Option<String>,
    /// Owning account of the currently selected mailbox when it is a
    /// shared folder reached through the `#user/<owner>/...` other-
    /// users namespace; `None` for a personal selection (the common
    /// case), which scopes to `account_id`. Every selected-mailbox
    /// read (FETCH / SEARCH / IDLE) scopes through
    /// [`Self::effective_account`], so a shared SELECT reads the
    /// owner's messages while the connection stays authenticated as
    /// the borrowing account. Cleared alongside `selected`.
    selected_account: Option<String>,
    /// RFC 4314 rights the authenticated account holds on the
    /// currently selected mailbox, as reported by `MYRIGHTS`. Set by
    /// SELECT/EXAMINE from the resolved mailbox, cleared alongside
    /// `selected`. Personal mailboxes get [`OWNER_RIGHTS`]; a shared
    /// folder gets whatever its `[[acl]]` grant confers, which is what
    /// makes the write gate below fixture-driven rather than a blanket
    /// "shared is read-only" rule.
    selected_rights: Option<String>,
    /// Account this connection is scoped to. Default = primary;
    /// LOGIN and AUTHENTICATE parse the supplied credential and
    /// rebind to a matching declared account if the username (or
    /// the bearer token, for XOAUTH2 / OAUTHBEARER) resolves. A
    /// credential that doesn't match any account leaves this on
    /// primary, matching the v0 "no auth in v0" baseline.
    account_id: String,
    /// Per-accepted-TCP-connection id stamped onto every entry this
    /// connection records into [`request_log`]. Allocated once per
    /// `serve_connection` call from `connection_id::next()`; lets
    /// harness scripts group entries by session (one LOGIN +
    /// N SELECTs vs N LOGINs + N SELECTs).
    connection_id: u64,
    /// Provider push hub. An `IDLE` command registers a waiter here for
    /// the connection's account; the test-admin state-mutation trigger
    /// (`POST /test/fixture/step` -> `PushHub::emit_state_advance`) wakes
    /// it so the idling client observes the change. Shared with the
    /// JMAP WebSocket / Gmail Pub/Sub / Graph webhook surfaces.
    push: crate::push::PushHub,
    /// Whether the client has `ENABLE QRESYNC`d this session (RFC 7162
    /// Section 3.1). Gates the expunge-report shape on `UID MOVE` /
    /// `UID EXPUNGE`: a QRESYNC-enabled connection gets `* VANISHED`,
    /// otherwise `* N EXPUNGE` (RFC 6851 Section 3 / RFC 7162 Section
    /// 3.2.10). bifrost's `MoveConsumer` selects which it reads from
    /// the same enabled state, so the two must agree.
    qresync_enabled: bool,
}

enum ReadOutcome {
    Line(String),
    PeerClosed,
    Shutdown,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    async fn write_line(&mut self, s: &str) -> std::io::Result<()> {
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.write_all(b"\r\n").await?;
        self.writer.flush().await
    }

    /// Read one CRLF-terminated line. Used both for top-level commands
    /// and for AUTHENTICATE continuation responses; the latter never
    /// races shutdown because the surrounding flow is bounded.
    async fn read_line(&mut self) -> std::io::Result<Option<String>> {
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf).await?;
        if n == 0 {
            Ok(None)
        } else {
            Ok(Some(strip_crlf(&buf).to_string()))
        }
    }

    /// Read the next top-level command line, racing against shutdown so
    /// SIGTERM during an idle connection exits cleanly.
    /// Acquire a brief read guard on the shared fixture. Callers must
    /// drop the guard before any `.await` or dispatcher callback;
    /// holding it across an await would block every subsequent
    /// connection attempting a write (`Email/set` / `Mailbox/set` on
    /// the JMAP listener) and risk deadlock with the dispatcher. In
    /// practice each command pulls owned data out of the guard
    /// (`Vec<ListEntry>`, `Counts`, etc.) and writes the wire bytes
    /// after the guard drops.
    fn fix_read(&self) -> std::sync::RwLockReadGuard<'_, Fixture> {
        self.fixture.read().expect("fixture lock poisoned")
    }

    async fn read_command_line(
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

    async fn dispatch(
        &mut self,
        line: &str,
        shutdown: &mut watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        let parsed = match parse_command_line(line) {
            Some(p) => p,
            None => {
                return self.write_line("* BAD malformed command line").await;
            }
        };

        let cmd_upper = parsed.command.to_ascii_uppercase();
        // For UID FETCH / UID SEARCH / etc. we record the
        // sub-command so test assertions can target the verb
        // ratatoskr actually issued, not just "UID". Per-command
        // record fires *once* per dispatched line, never per
        // matched message - so a `UID FETCH 1:*` against a 1M-row
        // fixture records a single entry, not a million. See the
        // FETCH handler for the per-message work.
        //
        // The bare-`UID` branch (no sub-command) records as
        // `"UID"`, which conflates a malformed line with a
        // legitimate-but-empty UID command. The dispatcher BADs
        // the response in either case so the conflation is
        // invisible to the wire client; tests that need to
        // distinguish should inspect `detail.args`.
        let recorded = if cmd_upper == "UID" {
            let sub = parsed
                .args
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if sub.is_empty() {
                "UID".to_string()
            } else {
                format!("UID {sub}")
            }
        } else {
            cmd_upper.clone()
        };
        // /test/requests is exposed unauthenticated, so for the
        // auth verbs we only record the mechanism (or an empty
        // string for LOGIN, which has no mechanism token).
        // `LOGIN <user> <pass>` and `AUTHENTICATE PLAIN <base64>`
        // would otherwise leak credentials verbatim. The `+`
        // continuation line for AUTHENTICATE is read inside
        // `cmd_authenticate` and never reaches `dispatch`, so no
        // redaction needed there.
        let logged_args: &str = if cmd_upper == "LOGIN" {
            ""
        } else if cmd_upper == "AUTHENTICATE" {
            parsed.args.split_whitespace().next().unwrap_or("")
        } else {
            parsed.args
        };
        // For `UID FETCH`, surface the parsed FETCH item list and a
        // `body` flag: `true` when any item asks for message bytes
        // (`BODY[...]`, `BODY.PEEK[...]`, `RFC822*`, part fetches),
        // `false` for metadata-only fetches (FLAGS, UID, MODSEQ,
        // INTERNALDATE, BODYSTRUCTURE, RFC822.SIZE). Lets a script
        // assert "no body refetch" while still permitting a flag-only
        // reconciliation pass. See `notes/request-log.md`.
        let mut detail = serde_json::json!({ "tag": parsed.tag, "args": logged_args });
        if recorded == "UID FETCH" {
            // At this layer `parsed.command == "UID"` and `parsed.args
            // == "FETCH <set> <attrs> [<modifiers>]"`. Drop the FETCH
            // sub-verb and the sequence-set, then strip any trailing
            // modifier list before parsing the attribute set.
            let attr_tail = parsed
                .args
                .split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ");
            let (attrs_str, _modifiers) = split_attrs_and_modifiers(&attr_tail);
            let attrs = parse_fetch_attrs(attrs_str).unwrap_or_default();
            let body = attrs.iter().any(|a| {
                matches!(
                    a,
                    FetchAttr::BodyFull
                        | FetchAttr::BodyHeader
                        | FetchAttr::BodyText
                        | FetchAttr::BodyPart(_)
                        | FetchAttr::BodyPartMime(_)
                )
            });
            let attr_names: Vec<String> = attrs.iter().map(fetch_attr_name).collect();
            detail["attrs"] = serde_json::json!(attr_names);
            detail["body"] = serde_json::json!(body);
        }
        self.request_log
            .record_with_conn("imap", recorded, detail, Some(self.connection_id));
        match cmd_upper.as_str() {
            "CAPABILITY" => self.cmd_capability(parsed.tag).await,
            "NOOP" => self.cmd_noop(parsed.tag).await,
            "LOGOUT" => self.cmd_logout(parsed.tag).await,
            "LOGIN" => self.cmd_login(parsed.tag, parsed.args).await,
            "AUTHENTICATE" => self.cmd_authenticate(parsed.tag, parsed.args).await,
            "ENABLE" => self.cmd_enable(parsed.tag, parsed.args).await,
            "NAMESPACE" => self.cmd_namespace(parsed.tag).await,
            "MYRIGHTS" => self.cmd_myrights(parsed.tag, parsed.args).await,
            "GETACL" => self.cmd_getacl(parsed.tag, parsed.args).await,
            "LIST" => self.cmd_list(parsed.tag, parsed.args).await,
            "CREATE" => self.cmd_create(parsed.tag, parsed.args).await,
            "RENAME" => self.cmd_rename(parsed.tag, parsed.args).await,
            "DELETE" => self.cmd_delete(parsed.tag, parsed.args).await,
            "STATUS" => self.cmd_status(parsed.tag, parsed.args).await,
            "SELECT" => self.cmd_select(parsed.tag, parsed.args, true).await,
            "EXAMINE" => self.cmd_select(parsed.tag, parsed.args, false).await,
            "UID" => self.cmd_uid(parsed.tag, parsed.args).await,
            "IDLE" => self.cmd_idle(parsed.tag, shutdown).await,
            "CLOSE" => self.cmd_close(parsed.tag).await,
            other => {
                self.write_line(&format!("{} BAD {other} not implemented in v0", parsed.tag))
                    .await
            }
        }
    }

    // ── Commands ────────────────────────────────────────────────────

    async fn cmd_capability(&mut self, tag: &str) -> std::io::Result<()> {
        self.write_line(&format!("* CAPABILITY {CAPABILITIES}"))
            .await?;
        self.write_line(&format!("{tag} OK CAPABILITY completed"))
            .await
    }

    async fn cmd_noop(&mut self, tag: &str) -> std::io::Result<()> {
        self.write_line(&format!("{tag} OK NOOP completed")).await
    }

    async fn cmd_logout(&mut self, tag: &str) -> std::io::Result<()> {
        self.write_line("* BYE saehrimnir signing off").await?;
        self.write_line(&format!("{tag} OK LOGOUT completed"))
            .await?;
        self.state = State::Logout;
        Ok(())
    }

    async fn cmd_login(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.state != State::NotAuthenticated {
            return self
                .write_line(&format!("{tag} BAD LOGIN only valid pre-auth"))
                .await;
        }
        // v0 accepts any credential EXCEPT the reserved rejection
        // sentinel. Parse both astrings: the user name binds the
        // connection to the matching `[[account]]` (an unrecognised
        // user stays on primary, matching the v0 "no auth" baseline),
        // and the password is checked against `REJECT_AUTH_PASSWORD`
        // so a harness script can deterministically drive an auth
        // failure.
        if let Some((user, pass)) = parse_two_astrings(args) {
            if is_reject_password(&pass) {
                return self
                    .write_line(&format!(
                        "{tag} NO [AUTHENTICATIONFAILED] LOGIN failed: invalid credentials"
                    ))
                    .await;
            }
            if let Some(id) = self.account_id_for_username(&user) {
                self.account_id = id;
            }
        }
        self.state = State::Authenticated;
        self.write_line(&format!(
            "{tag} OK [CAPABILITY {CAPABILITIES}] LOGIN completed"
        ))
        .await
    }

    /// Look up the account a SASL identity binds the connection to.
    /// `identity` is typically email-shaped (`alice@example.com`);
    /// case-insensitive match against `account.name`. None when no
    /// account matches; the caller leaves `self.account_id` at its
    /// current value (primary by default).
    fn account_id_for_username(&self, identity: &str) -> Option<String> {
        let fixture = self.fix_read();
        let trimmed = identity.trim();
        fixture
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(trimmed))
            .map(|a| a.id.clone())
    }

    /// Resolve a bearer token (XOAUTH2 / OAUTHBEARER) to the account
    /// it authorizes. Falls back to None for unknown tokens so the
    /// caller can leave the connection on whichever account it was
    /// already bound to.
    fn account_id_for_bearer(&self, token: &str) -> Option<String> {
        self.token_store.account_for_token(token)
    }

    async fn cmd_authenticate(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.state != State::NotAuthenticated {
            return self
                .write_line(&format!("{tag} BAD AUTHENTICATE only valid pre-auth"))
                .await;
        }
        let mut parts = args.splitn(2, ' ');
        let mech = parts.next().unwrap_or("").to_ascii_uppercase();
        let initial_response = parts.next().map(|s| s.trim().to_string());
        match mech.as_str() {
            "PLAIN" | "XOAUTH2" | "OAUTHBEARER" | "LOGIN" => {
                // SASL-IR: if the client sent the response on the same
                // line, no continuation needed. Otherwise prompt with
                // `+\r\n` and read one continuation line.
                let response_b64 = match initial_response {
                    Some(s) => Some(s),
                    None => {
                        self.write_line("+").await?;
                        let cont = self.read_line().await?;
                        if cont.is_none() {
                            return Ok(()); // peer closed mid-handshake
                        }
                        let cont = cont.unwrap_or_default();
                        if cont.trim() == "*" {
                            return self
                                .write_line(&format!("{tag} BAD AUTHENTICATE aborted"))
                                .await;
                        }
                        Some(cont)
                    }
                };
                // Parse the SASL response for an account-binding hint.
                // PLAIN: base64(`\0user\0pass`).
                // XOAUTH2 / OAUTHBEARER: base64 of a key-value blob
                //   containing `user=<email>` and `auth=Bearer <tok>`.
                // LOGIN: the first continuation carries `user` (we
                //   don't model the second `+` round-trip; tests
                //   that need it can extend later).
                if let Some(b64) = response_b64
                    && let Some(decoded) = sasl_decode_b64(&b64)
                {
                    // Reserved rejection sentinel: a PLAIN response
                    // carrying `REJECT_AUTH_PASSWORD` fails auth so a
                    // harness script can drive the same bad-password
                    // failure over AUTHENTICATE that LOGIN exposes.
                    if let Some(pass) = sasl_extract_password(mech.as_str(), &decoded)
                        && is_reject_password(&pass)
                    {
                        return self
                            .write_line(&format!(
                                "{tag} NO [AUTHENTICATIONFAILED] {mech} failed: invalid credentials"
                            ))
                            .await;
                    }
                    if let Some(token) = sasl_extract_bearer(&decoded)
                        && let Some(id) = self.account_id_for_bearer(&token)
                    {
                        self.account_id = id;
                    } else if let Some(user) = sasl_extract_username(mech.as_str(), &decoded)
                        && let Some(id) = self.account_id_for_username(&user)
                    {
                        self.account_id = id;
                    }
                }
                self.state = State::Authenticated;
                self.write_line(&format!(
                    "{tag} OK [CAPABILITY {CAPABILITIES}] {mech} authentication accepted"
                ))
                .await
            }
            "" => {
                self.write_line(&format!("{tag} BAD AUTHENTICATE missing mechanism"))
                    .await
            }
            other => {
                self.write_line(&format!("{tag} NO unsupported SASL mechanism {other:?}"))
                    .await
            }
        }
    }

    async fn cmd_enable(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.state == State::NotAuthenticated {
            return self
                .write_line(&format!("{tag} BAD ENABLE requires authentication"))
                .await;
        }
        // Echo back any extension we recognise. Ratatoskr only sends
        // QRESYNC; anything else we silently drop from the response so
        // the client can detect non-support.
        let mut enabled = Vec::new();
        for ext in args.split_whitespace() {
            if ext.eq_ignore_ascii_case("QRESYNC") {
                enabled.push("QRESYNC");
                // Latch QRESYNC so the expunge-report shape on a later
                // UID MOVE / UID EXPUNGE switches to `* VANISHED`.
                self.qresync_enabled = true;
            } else if ext.eq_ignore_ascii_case("CONDSTORE") {
                enabled.push("CONDSTORE");
            }
        }
        if !enabled.is_empty() {
            self.write_line(&format!("* ENABLED {}", enabled.join(" ")))
                .await?;
        }
        self.write_line(&format!("{tag} OK ENABLE completed")).await
    }

    async fn cmd_list(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD LIST requires authentication"))
                .await;
        }
        let (reference, pattern) = match parse_two_astrings(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD LIST expects \"reference\" \"pattern\""))
                    .await;
            }
        };
        // RFC 3501 sec 6.3.8: empty pattern with empty reference asks
        // for the hierarchy delimiter only.
        if pattern.is_empty() {
            self.write_line("* LIST (\\Noselect) \"/\" \"\"").await?;
            return self.write_line(&format!("{tag} OK LIST completed")).await;
        }
        // We accept both `*` and exact-name patterns; everything else
        // falls back to substring matching, which is enough for
        // ratatoskr (it only ever sends `*`).
        let _ = reference; // hierarchy reference is unused in v0.
        let mut entries = list_mailboxes(&self.fix_read(), &self.account_id);
        // Other-users namespace: a `LIST "" "#user/*"` (or any pattern
        // naming the `#user/` prefix) enumerates mailboxes other
        // accounts have shared with this one. A bare `LIST "" "*"`
        // stays personal-only - matching real servers, where the
        // other-users namespace is not walked unless explicitly named
        // - so shared entries are appended only when the pattern
        // references the prefix.
        if pattern.contains(SHARED_NAMESPACE_PREFIX) {
            entries.extend(
                list_shared_mailboxes(&self.fix_read(), &self.account_id)
                    .into_iter()
                    .map(|s| ListEntry {
                        fixture_id: s.fixture_id,
                        path: s.path,
                        attributes: s.attributes,
                    }),
            );
        }
        for e in &entries {
            if !pattern_matches(&pattern, &e.path) {
                continue;
            }
            let attrs = if e.attributes.is_empty() {
                String::new()
            } else {
                e.attributes.join(" ")
            };
            self.write_line(&format!("* LIST ({attrs}) \"/\" \"{}\"", e.path))
                .await?;
        }
        self.write_line(&format!("{tag} OK LIST completed")).await
    }

    /// `NAMESPACE` (RFC 2342). Advertises the personal namespace
    /// (empty prefix, `/` delimiter), the other-users namespace
    /// (`#user/` prefix) that surfaces shared folders, and a NIL
    /// shared namespace. bifrost walks the raw response line to learn
    /// the other-users prefix before `LIST`ing shared folders.
    ///
    /// The other-users namespace is advertised only when the fixture
    /// can ever share a mailbox (`imap_advertises_other_namespace`:
    /// static ACLs, scripted `acl_grant`s, or a non-personal account).
    /// A personal-only fixture answers personal-namespace-only, so
    /// clients see the common personal-server shape and never learn a
    /// `#user/` root that could not possibly grow folders.
    async fn cmd_namespace(&mut self, tag: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD NAMESPACE requires authentication"))
                .await;
        }
        let advertises_other = self
            .fixture
            .read()
            .expect("fixture lock poisoned")
            .imap_advertises_other_namespace();
        let line = if advertises_other {
            format!("* NAMESPACE ((\"\" \"/\")) ((\"{SHARED_NAMESPACE_PREFIX}\" \"/\")) NIL")
        } else {
            "* NAMESPACE ((\"\" \"/\")) NIL NIL".to_string()
        };
        self.write_line(&line).await?;
        self.write_line(&format!("{tag} OK NAMESPACE completed"))
            .await
    }

    /// `MYRIGHTS <mailbox>` (RFC 4314). Reports the rights the
    /// authenticated account holds on the named mailbox - full
    /// [`OWNER_RIGHTS`] for a personal mailbox it owns, or the granted
    /// rights for a `#user/<owner>/...` shared folder. `NO` when the
    /// mailbox doesn't resolve or the account holds no rights on it.
    async fn cmd_myrights(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD MYRIGHTS requires authentication"))
                .await;
        }
        let Some(name) = parse_one_astring(args) else {
            return self
                .write_line(&format!("{tag} BAD MYRIGHTS expects \"mailbox\""))
                .await;
        };
        let Some(resolved) = self.resolve_mailbox(&name) else {
            return self
                .write_line(&format!(
                    "{tag} NO MYRIGHTS unknown or inaccessible mailbox"
                ))
                .await;
        };
        self.write_line(&format!(
            "* MYRIGHTS \"{}\" {}",
            resolved.path, resolved.rights
        ))
        .await?;
        self.write_line(&format!("{tag} OK MYRIGHTS completed"))
            .await
    }

    /// `GETACL <mailbox>` (RFC 4314). Lists every identifier and its
    /// rights on the mailbox: the owning account (always
    /// [`OWNER_RIGHTS`]) plus each declared `[[acl]]` grant. The
    /// authenticated account must itself hold rights on the mailbox to
    /// read the ACL (v0 treats any held right as sufficient - real
    /// servers gate on the `a` administer right).
    async fn cmd_getacl(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD GETACL requires authentication"))
                .await;
        }
        let Some(name) = parse_one_astring(args) else {
            return self
                .write_line(&format!("{tag} BAD GETACL expects \"mailbox\""))
                .await;
        };
        let Some(resolved) = self.resolve_mailbox(&name) else {
            return self
                .write_line(&format!("{tag} NO GETACL unknown or inaccessible mailbox"))
                .await;
        };
        // Owner identifier first (full rights), then each grant in
        // declared order. Read from owned data so the guard drops
        // before the write.
        let pairs: Vec<(String, String)> = {
            let fix = self.fix_read();
            let owner_name = fix
                .account(&resolved.account_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| resolved.account_id.clone());
            let mut pairs = vec![(owner_name, OWNER_RIGHTS.to_string())];
            for g in fix.acls_for_mailbox(&resolved.fixture_id) {
                let ident = fix
                    .account(&g.identifier)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| g.identifier.clone());
                pairs.push((ident, g.rights.clone()));
            }
            pairs
        };
        let rendered: String = pairs
            .iter()
            .map(|(id, rights)| format!("{id} {rights}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.write_line(&format!("* ACL \"{}\" {rendered}", resolved.path))
            .await?;
        self.write_line(&format!("{tag} OK GETACL completed")).await
    }

    /// The account whose messages the currently selected mailbox
    /// belongs to: the shared folder's owner when one is selected,
    /// otherwise the connection's own account. Every selected-mailbox
    /// read scopes through this.
    fn effective_account(&self) -> &str {
        self.selected_account.as_deref().unwrap_or(&self.account_id)
    }

    /// True when the authenticated account holds every RFC 4314 right
    /// in `needed` on the current selection. A personal selection has
    /// no recorded rights string and is always permitted (the owner
    /// holds [`OWNER_RIGHTS`] implicitly).
    fn holds_rights(&self, needed: &str) -> bool {
        match &self.selected_rights {
            Some(held) => needed.chars().all(|c| held.contains(c)),
            None => true,
        }
    }

    /// Reject a mutating command the current selection's rights do not
    /// cover. `needed` is the RFC 4314 right(s) the command requires
    /// (`w` write flags, `i` insert, `t` delete-message, `e` expunge);
    /// the held set comes from the fixture `[[acl]]` grant, so one
    /// fixture can stage a read-only shared folder (`lr`) next to a
    /// writable one (`lrswipkxte`) and the two behave differently.
    ///
    /// Returns `Ok(true)` when it wrote the rejection (caller returns
    /// immediately), `Ok(false)` when the command may proceed. A
    /// personal selection always proceeds.
    async fn reject_shared_write(
        &mut self,
        tag: &str,
        cmd: &str,
        needed: &str,
    ) -> std::io::Result<bool> {
        if self.selected_account.is_some() && !self.holds_rights(needed) {
            self.write_line(&format!(
                "{tag} NO [NOPERM] {cmd} not permitted on this shared folder (requires {needed:?}, holds {:?})",
                self.selected_rights.as_deref().unwrap_or("")
            ))
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve a mailbox name - personal (`INBOX`, `Work/Reports`) or
    /// shared (`#user/<owner>/<path>`) - to the owning account, the
    /// fixture mailbox id, the canonical wire path, and the rights the
    /// authenticated account holds on it. `None` when the name doesn't
    /// resolve or the account has no access (no ACL grant on a shared
    /// folder).
    fn resolve_mailbox(&self, name: &str) -> Option<ResolvedMailbox> {
        let fix = self.fix_read();
        if name.starts_with(SHARED_NAMESPACE_PREFIX) {
            let shared = list_shared_mailboxes(&fix, &self.account_id);
            let entry = shared
                .into_iter()
                .find(|s| s.path.eq_ignore_ascii_case(name))?;
            Some(ResolvedMailbox {
                account_id: entry.owner_account_id,
                fixture_id: entry.fixture_id,
                path: entry.path,
                rights: entry.rights,
            })
        } else {
            let entries = list_mailboxes(&fix, &self.account_id);
            let entry = entries
                .into_iter()
                .find(|e| e.path.eq_ignore_ascii_case(name))?;
            Some(ResolvedMailbox {
                account_id: self.account_id.clone(),
                fixture_id: entry.fixture_id,
                path: entry.path,
                rights: OWNER_RIGHTS.to_string(),
            })
        }
    }

    async fn cmd_status(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD STATUS requires authentication"))
                .await;
        }
        let parsed = match parse_status_args(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD STATUS expects \"name\" (item ...)"))
                    .await;
            }
        };
        let entries = list_mailboxes(&self.fix_read(), &self.account_id);
        let entry = entries
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case(&parsed.name));
        let Some(entry) = entry else {
            return self
                .write_line(&format!("{tag} NO STATUS unknown mailbox"))
                .await;
        };
        let counts = mailbox_counts(&self.fix_read(), &self.account_id, &entry.fixture_id);
        let mut items = Vec::with_capacity(parsed.items.len());
        for item in &parsed.items {
            let pair = match item.to_ascii_uppercase().as_str() {
                "MESSAGES" => format!("MESSAGES {}", counts.exists),
                "UNSEEN" => format!("UNSEEN {}", counts.unseen),
                "RECENT" => "RECENT 0".to_string(),
                "UIDNEXT" => format!("UIDNEXT {}", counts.uidnext),
                "UIDVALIDITY" => "UIDVALIDITY 1".to_string(),
                "HIGHESTMODSEQ" => {
                    let hms = self.fix_read().imap_highestmodseq(&self.account_id);
                    format!("HIGHESTMODSEQ {hms}")
                }
                _ => continue,
            };
            items.push(pair);
        }
        self.write_line(&format!(
            "* STATUS \"{}\" ({})",
            entry.path,
            items.join(" ")
        ))
        .await?;
        self.write_line(&format!("{tag} OK STATUS completed")).await
    }

    fn is_authenticated(&self) -> bool {
        matches!(self.state, State::Authenticated | State::Selected)
    }

    async fn cmd_select(&mut self, tag: &str, args: &str, read_write: bool) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD SELECT requires authentication"))
                .await;
        }
        // RFC 7162 select-parameters ride after the mailbox name:
        // bifrost sends `SELECT INBOX (CONDSTORE)`, and a QRESYNC-capable
        // client may send `SELECT INBOX (QRESYNC (<uidvalidity> <modseq>
        // [<known-uids>]))`. Parsing them (vs. the old name-only astring,
        // which rejected the trailing paren group as junk and replied
        // BAD) is what lets bifrost's real CONDSTORE SELECT land.
        let select = match parse_select_args(args) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD SELECT expects \"name\" [(modifiers)]"))
                    .await;
            }
        };
        let name = select.name.clone();
        // Resolve personal or `#user/...` shared name to its owning
        // account + fixture id. A shared name the viewer holds no
        // grant on resolves to None -> NO, so cross-principal access
        // without an ACL is refused.
        let Some(resolved) = self.resolve_mailbox(&name) else {
            // Per RFC, on NO SELECT the connection drops back to
            // Authenticated state.
            self.state = State::Authenticated;
            self.selected = None;
            self.selected_account = None;
            self.selected_rights = None;
            // A path in the other-users namespace can name a real mailbox
            // whose ACL was withdrawn after the client discovered it. Keep
            // that distinct from an invented mailbox: bifrost maps NOPERM
            // on a previously shared scope to ScopeRevoked, which disables
            // only that scope. Reporting it as an unqualified NO loses the
            // permission class and incorrectly turns the account terminal.
            if shared_mailbox_path_exists(&self.fix_read(), &self.account_id, &name) {
                return self
                    .write_line(&format!("{tag} NO [NOPERM] SELECT access revoked"))
                    .await;
            }
            return self
                .write_line(&format!("{tag} NO SELECT unknown mailbox"))
                .await;
        };
        // Scope every projection below to the mailbox's owning account
        // (the shared folder's owner, or this connection's own account
        // for a personal mailbox).
        let owner = resolved.account_id.clone();
        let entry_id = resolved.fixture_id.clone();
        let rights = resolved.rights.clone();

        // Counts and the first-unseen index are both pure projections
        // into owned data; each helper borrows the fixture only for
        // its own call and the result owns no `&Fixture` references,
        // so the guard drops between calls.
        let counts = mailbox_counts(&self.fix_read(), &owner, &entry_id);
        let first_unseen_idx = {
            let fix = self.fix_read();
            mailbox_messages(&fix, &owner, &entry_id)
                .iter()
                .position(|(_, e)| !e.keywords.iter().any(|k| k == "$seen"))
        };

        // Untagged responses, RFC 3501 sec 6.3.1. Order does not
        // matter, but we emit a stable order for byte-determinism.
        self.write_line(&format!("* {} EXISTS", counts.exists))
            .await?;
        self.write_line("* 0 RECENT").await?;
        self.write_line("* FLAGS (\\Seen \\Flagged \\Draft \\Answered \\Deleted)")
            .await?;
        self.write_line(
            "* OK [PERMANENTFLAGS (\\Seen \\Flagged \\Draft \\Answered \\Deleted \\*)] flags writable",
        )
        .await?;
        if let Some(idx) = first_unseen_idx {
            self.write_line(&format!("* OK [UNSEEN {}] first unseen", idx + 1))
                .await?;
        }
        self.write_line("* OK [UIDVALIDITY 1] folder identity")
            .await?;
        self.write_line(&format!(
            "* OK [UIDNEXT {}] predicted next UID",
            counts.uidnext
        ))
        .await?;
        let highestmodseq = self.fix_read().imap_highestmodseq(&owner);
        self.write_line(&format!("* OK [HIGHESTMODSEQ {highestmodseq}] modseq"))
            .await?;

        // RFC 7162 QRESYNC: when the client opened with a
        // `(QRESYNC (...))` parameter, report the UIDs that have been
        // expunged so it can prune its cache without a full `UID SEARCH
        // ALL` diff. We resolve "gone" straight from the mailbox's UID
        // history (a retired slot is `None`), bounded to the client's
        // known-UID set when it supplied one (the optional 3rd QRESYNC
        // element). No known set -> report every gone slot; the client
        // ignores UIDs it never knew.
        if let Some(qr) = &select.qresync {
            let gone = expunged_uids(&self.fix_read(), &entry_id, qr.known_uids.as_ref());
            if !gone.is_empty() {
                self.write_line(&format!(
                    "* VANISHED (EARLIER) {}",
                    format_uid_ranges(&gone)
                ))
                .await?;
            }
        }

        // A SELECT the account holds no write-shaped right on opens
        // READ-ONLY, which is how a client learns a shared folder is
        // read-only without a separate MYRIGHTS round trip. Any of
        // insert / write-flags / seen / expunge / delete-message
        // counts as writable. Personal mailboxes carry OWNER_RIGHTS,
        // so this only ever downgrades a shared selection.
        let writable = "iwset".chars().any(|c| rights.contains(c));
        let access = if read_write && writable {
            "READ-WRITE"
        } else {
            "READ-ONLY"
        };
        // The verb names the command that was issued, not the access
        // level granted: an EXAMINE always reports EXAMINE, and a
        // SELECT downgraded to READ-ONLY still reports SELECT.
        let verb = if read_write { "SELECT" } else { "EXAMINE" };
        self.state = State::Selected;
        self.selected = Some(entry_id);
        self.selected_rights = Some(rights);
        // Remember the owning account only for a shared selection; a
        // personal selection leaves it None so `effective_account`
        // falls back to `account_id`.
        self.selected_account = if owner == self.account_id {
            None
        } else {
            Some(owner)
        };
        self.write_line(&format!("{tag} OK [{access}] {verb} completed"))
            .await
    }

    async fn cmd_close(&mut self, tag: &str) -> std::io::Result<()> {
        if self.state != State::Selected {
            return self
                .write_line(&format!("{tag} BAD CLOSE requires SELECT"))
                .await;
        }
        self.selected = None;
        self.selected_account = None;
        self.selected_rights = None;
        self.state = State::Authenticated;
        self.write_line(&format!("{tag} OK CLOSE completed")).await
    }

    /// `CREATE <mailbox>` (RFC 3501 6.3.3). Creates a mailbox in the
    /// requesting account, persisting it into the shared fixture so a
    /// subsequent `LIST` reflects it. `/` is the hierarchy delimiter
    /// (matching our `LIST` output): the all-but-last segments resolve
    /// to the parent mailbox, the final segment is the new leaf name.
    /// bifrost's `container_create` sends the fully qualified path.
    async fn cmd_create(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD CREATE requires authentication"))
                .await;
        }
        let Some(name) = parse_one_astring(args) else {
            return self
                .write_line(&format!("{tag} BAD CREATE expects \"mailbox\""))
                .await;
        };
        // RFC 3501: a trailing hierarchy delimiter requests a
        // superior-only mailbox; treat it as the same leaf name.
        let name = name.trim_end_matches('/').to_string();
        if name.is_empty() {
            return self
                .write_line(&format!("{tag} NO CREATE empty mailbox name"))
                .await;
        }
        let outcome: Result<(), String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let entries = list_mailboxes(&fix, &self.account_id);
            if entries.iter().any(|e| e.path.eq_ignore_ascii_case(&name)) {
                Err(format!("CREATE mailbox {name:?} already exists"))
            } else {
                let (parent_path, leaf) = split_parent_leaf(&name);
                let parent_resolved = match parent_path {
                    None => Ok(None),
                    Some(pp) => entries
                        .iter()
                        .find(|e| e.path.eq_ignore_ascii_case(pp))
                        .map(|e| Some(e.fixture_id.clone()))
                        .ok_or_else(|| format!("CREATE unknown parent for {name:?}")),
                };
                match parent_resolved {
                    Err(e) => Err(e),
                    Ok(parent_id) => {
                        let new_id = fix.fresh_mailbox_id();
                        let account_id = self.account_id.clone();
                        let leaf = leaf.to_string();
                        let created_id = new_id.clone();
                        let _ = fix.mutate(move |f| {
                            let mut diff = crate::fixture::MutationDiff::default();
                            f.mailboxes.push(Mailbox {
                                id: new_id,
                                account_id,
                                name: leaf,
                                role: None,
                                parent_id,
                                sort_order: None,
                                is_subscribed: true,
                            });
                            diff.mailbox_created.push(created_id);
                            diff
                        });
                        Ok(())
                    }
                }
            }
        };
        match outcome {
            Ok(()) => self.write_line(&format!("{tag} OK CREATE completed")).await,
            Err(reason) => self.write_line(&format!("{tag} NO {reason}")).await,
        }
    }

    /// `RENAME <old> <new>` (RFC 3501 6.3.5). Resolves the existing
    /// mailbox by path, then sets its leaf name and re-parents it to
    /// match the new path - so RENAME doubles as a move when the new
    /// path has a different parent (this is exactly how bifrost's
    /// `container_move` re-targets a folder: a RENAME to a sibling
    /// path under a new parent). Children follow automatically since
    /// they reference the parent by id, not by path. Persisted into the
    /// fixture so a subsequent `LIST` reflects the change.
    async fn cmd_rename(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD RENAME requires authentication"))
                .await;
        }
        let Some((old_name, new_name)) = parse_two_astrings(args) else {
            return self
                .write_line(&format!("{tag} BAD RENAME expects \"old\" \"new\""))
                .await;
        };
        let new_name = new_name.trim_end_matches('/').to_string();
        let outcome: Result<(), String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let entries = list_mailboxes(&fix, &self.account_id);
            let target = entries
                .iter()
                .find(|e| e.path.eq_ignore_ascii_case(&old_name))
                .map(|e| e.fixture_id.clone());
            match target {
                None => Err(format!("RENAME unknown mailbox {old_name:?}")),
                Some(id) => {
                    if entries
                        .iter()
                        .any(|e| e.path.eq_ignore_ascii_case(&new_name))
                    {
                        Err(format!("RENAME target {new_name:?} already exists"))
                    } else {
                        let (parent_path, leaf) = split_parent_leaf(&new_name);
                        let parent_resolved = match parent_path {
                            None => Ok(None),
                            Some(pp) => entries
                                .iter()
                                .find(|e| e.path.eq_ignore_ascii_case(pp))
                                .map(|e| Some(e.fixture_id.clone()))
                                .ok_or_else(|| format!("RENAME unknown parent for {new_name:?}")),
                        };
                        match parent_resolved {
                            Err(e) => Err(e),
                            Ok(parent_id) => {
                                let leaf = leaf.to_string();
                                let updated_id = id.clone();
                                let _ = fix.mutate(move |f| {
                                    let mut diff = crate::fixture::MutationDiff::default();
                                    if let Some(m) = f.mailboxes.iter_mut().find(|m| m.id == id) {
                                        m.name = leaf;
                                        m.parent_id = parent_id;
                                        diff.mailbox_updated.push(updated_id);
                                    }
                                    diff
                                });
                                Ok(())
                            }
                        }
                    }
                }
            }
        };
        match outcome {
            Ok(()) => self.write_line(&format!("{tag} OK RENAME completed")).await,
            Err(reason) => self.write_line(&format!("{tag} NO {reason}")).await,
        }
    }

    /// `DELETE <mailbox>` (RFC 3501 6.3.4). Resolves by path and drops
    /// the mailbox from the fixture set so a subsequent `LIST` no longer
    /// reports it. bifrost guards with a `STATUS ... MESSAGES` first and
    /// refuses to delete a non-empty mailbox, so v0 does not need to
    /// cascade message deletion. A mailbox with children is refused
    /// (RFC 3501: deleting a `\Noselect` parent is a `NO`).
    async fn cmd_delete(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD DELETE requires authentication"))
                .await;
        }
        let Some(name) = parse_one_astring(args) else {
            return self
                .write_line(&format!("{tag} BAD DELETE expects \"mailbox\""))
                .await;
        };
        let name = name.trim_end_matches('/').to_string();
        let outcome: Result<(), String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let entries = list_mailboxes(&fix, &self.account_id);
            let target = entries
                .iter()
                .find(|e| e.path.eq_ignore_ascii_case(&name))
                .map(|e| e.fixture_id.clone());
            match target {
                None => Err(format!("DELETE unknown mailbox {name:?}")),
                Some(id) => {
                    let has_children = fix
                        .mailboxes
                        .iter()
                        .any(|m| m.parent_id.as_deref() == Some(id.as_str()));
                    if has_children {
                        Err(format!("DELETE mailbox {name:?} has inferior mailboxes"))
                    } else {
                        let account_id = fix
                            .mailboxes
                            .iter()
                            .find(|m| m.id == id)
                            .map(|m| m.account_id.clone());
                        let destroyed_id = id.clone();
                        let _ = fix.mutate(move |f| {
                            let mut diff = crate::fixture::MutationDiff::default();
                            let len_before = f.mailboxes.len();
                            f.mailboxes.retain(|m| m.id != id);
                            if f.mailboxes.len() < len_before {
                                diff.mailbox_destroyed.push(destroyed_id);
                                diff.mailbox_destroyed_accounts
                                    .push(account_id.expect("mailbox existed before retain"));
                            }
                            diff
                        });
                        Ok(())
                    }
                }
            }
        };
        match outcome {
            Ok(()) => self.write_line(&format!("{tag} OK DELETE completed")).await,
            Err(reason) => self.write_line(&format!("{tag} NO {reason}")).await,
        }
    }

    /// `IDLE` (RFC 2177). Valid only in the Selected state. Replies
    /// `+ idling`, then parks: a wake from the shared push hub (driven
    /// by the test-admin state-mutation trigger, the same path that
    /// fires the JMAP WebSocket / Gmail Pub/Sub / Graph webhook
    /// surfaces) makes the connection recompute the selected mailbox's
    /// live message set and emit the unsolicited untagged responses a
    /// real server sends - `* n EXPUNGE` for vanished messages (highest
    /// sequence first per RFC 3501 so no renumbering is needed) and
    /// `* n EXISTS` + `* n RECENT` when messages arrive. `DONE` ends the
    /// idle with a tagged OK; a SIGTERM-driven shutdown emits `* BYE`
    /// and closes.
    async fn cmd_idle(
        &mut self,
        tag: &str,
        shutdown: &mut watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        if self.state != State::Selected {
            return self
                .write_line(&format!("{tag} BAD IDLE requires SELECT first"))
                .await;
        }
        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");
        // Register a waiter on the shared push hub for this connection's
        // account, and snapshot the mailbox's current live-UID view so
        // the first wake diffs against the pre-IDLE state. The receiver
        // drops when this method returns (DONE / shutdown / peer close);
        // the hub prunes the stale registration on its next emit.
        // Wake on the *owning* account's state advance (the shared
        // folder's owner for a shared selection, else our own).
        let mut idle_rx = self
            .push
            .register_imap_idle(self.effective_account().to_string());
        let mut last_uids: Vec<u32> = {
            let fix = self.fix_read();
            live_uids(&fix, self.effective_account(), &selected_id)
        };
        self.write_line("+ idling").await?;
        loop {
            let mut line = String::new();
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let _ = self.write_line("* BYE saehrimnir shutting down").await;
                        self.state = State::Logout;
                        return Ok(());
                    }
                }
                r = self.reader.read_line(&mut line) => {
                    let n = r?;
                    if n == 0 {
                        // Peer closed mid-idle; end the connection.
                        self.state = State::Logout;
                        return Ok(());
                    }
                    if strip_crlf(&line).trim().eq_ignore_ascii_case("DONE") {
                        return self
                            .write_line(&format!("{tag} OK IDLE terminated"))
                            .await;
                    }
                    // RFC 2177: only DONE is valid while idling. Anything
                    // else is ignored (we keep idling) rather than
                    // desynchronising the connection.
                }
                _ = idle_rx.recv() => {
                    self.emit_idle_updates(&selected_id, &mut last_uids).await?;
                }
            }
        }
    }

    /// Recompute the selected mailbox's live message set and emit the
    /// untagged responses for whatever changed since `last_uids`, then
    /// update `last_uids`. Diffing against the last-emitted view (rather
    /// than acting on a wake payload) keeps the output correct even if
    /// several mutations coalesced into one wake.
    async fn emit_idle_updates(
        &mut self,
        selected_id: &str,
        last_uids: &mut Vec<u32>,
    ) -> std::io::Result<()> {
        let new_uids: Vec<u32> = {
            let fix = self.fix_read();
            live_uids(&fix, self.effective_account(), selected_id)
        };
        // Expunged: UIDs present before but gone now. The IMAP sequence
        // number is the 1-based position in the *old* live view; emit
        // highest-first so earlier emissions don't renumber later ones
        // (matches `cmd_uid_expunge`).
        let mut expunged_seqs: Vec<u32> = last_uids
            .iter()
            .enumerate()
            .filter(|(_, uid)| !new_uids.contains(uid))
            .map(|(i, _)| u32::try_from(i + 1).expect("seq fits in u32"))
            .collect();
        expunged_seqs.sort_unstable_by_key(|s| std::cmp::Reverse(*s));
        for seq in &expunged_seqs {
            self.write_line(&format!("* {seq} EXPUNGE")).await?;
        }
        // Arrivals: UIDs new since the last view. A real server reports
        // the new mailbox total via EXISTS plus a RECENT count for the
        // freshly-arrived messages.
        let arrivals = new_uids
            .iter()
            .filter(|uid| !last_uids.contains(uid))
            .count();
        if arrivals > 0 {
            self.write_line(&format!("* {} EXISTS", new_uids.len()))
                .await?;
            self.write_line(&format!("* {arrivals} RECENT")).await?;
        }
        *last_uids = new_uids;
        Ok(())
    }

    async fn cmd_uid(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.state != State::Selected {
            return self
                .write_line(&format!("{tag} BAD UID requires SELECT"))
                .await;
        }
        let mut parts = args.splitn(2, ' ');
        let sub = parts.next().unwrap_or("").to_ascii_uppercase();
        let rest = parts.next().unwrap_or("").trim();
        match sub.as_str() {
            "SEARCH" => self.cmd_uid_search(tag, rest).await,
            "FETCH" => self.cmd_uid_fetch(tag, rest).await,
            "STORE" => self.cmd_uid_store(tag, rest).await,
            "COPY" => self.cmd_uid_copy(tag, rest).await,
            "MOVE" => self.cmd_uid_move(tag, rest).await,
            "EXPUNGE" => self.cmd_uid_expunge(tag, rest).await,
            other => {
                self.write_line(&format!("{tag} BAD UID {other} not implemented in v0"))
                    .await
            }
        }
    }

    async fn cmd_uid_fetch(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        // Args: "<uid-set> (<attr>...)" or "<uid-set> <attr>" or
        // "<uid-set> (<attrs>) (CHANGEDSINCE <modseq>)". CHANGEDSINCE is
        // honoured per-message against the real per-email modseq (see
        // the snapshot filter below), so a mutated message surfaces in
        // the next delta FETCH while an untouched one is skipped.
        let (uid_set_str, after_set) = match split_after_set(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID FETCH expects <set> <attrs>"))
                    .await;
            }
        };
        let set = match parse_uid_set(uid_set_str) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID FETCH bad sequence-set"))
                    .await;
            }
        };
        let (attrs_str, modifiers_str) = split_attrs_and_modifiers(after_set);
        let mut attrs = match parse_fetch_attrs(attrs_str) {
            Some(a) => a,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID FETCH bad attribute list"))
                    .await;
            }
        };
        let changedsince = match parse_changedsince(modifiers_str) {
            Ok(v) => v,
            Err(()) => {
                return self
                    .write_line(&format!("{tag} BAD UID FETCH bad modifier list"))
                    .await;
            }
        };
        // RFC 7162 3.1.4.1: when CHANGEDSINCE is present the server MUST
        // include MODSEQ in every FETCH response, even if the client did
        // not list it. bifrost asks for `(FLAGS) (CHANGEDSINCE n)` and
        // relies on the SELECT-level HIGHESTMODSEQ, but emitting the
        // per-message MODSEQ keeps the wire RFC-correct for any client
        // that advances a per-message cache.
        if changedsince.is_some() && !attrs.contains(&FetchAttr::ModSeq) {
            attrs.push(FetchAttr::ModSeq);
        }

        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");

        // Reactive callback: a registered `on("imap", "UID FETCH",
        // ...)` handler can override the response. `nil` return =
        // pass through; a `{status = "...", message = "..."}` table
        // = emit just the tagged response (no FETCH untagged
        // updates).
        if let Some(d) = &self.dispatcher {
            let uid_set_owned = uid_set_str.to_string();
            let attrs_owned = attrs_str.to_string();
            let mailbox = selected_id.clone();
            let result = d.dispatch("imap", "UID FETCH", move |state| {
                crate::lua::req_set_str(state, "uid_set", &uid_set_owned)?;
                crate::lua::req_set_str(state, "attrs", &attrs_owned)?;
                crate::lua::req_set_str(state, "mailbox", &mailbox)?;
                Ok(())
            });
            if let crate::lua::Override::Tagged { status, message } = result {
                return self.write_line(&format!("{tag} {status} {message}")).await;
            }
        }

        // Snapshot the rendered FETCH lines up front so the read
        // Snapshot just the (seq, uid, Email) triples we'll
        // render under one read guard. Holding owned Email clones
        // (vs. holding the read guard across the writes) keeps the
        // determinism contract: every FETCH response on this line
        // sees the same fixture image. Pre-fix we materialised the
        // fully-rendered FETCH lines upfront; for a 10k-message
        // FETCH with bodies that peaked RAM at the entire mailbox
        // of rendered text. Snapshotting Email clones then
        // rendering+writing one at a time lets each entry's render
        // strings get reclaimed before the next is built.
        //
        // Sequence number is the message's position in the live
        // mailbox view (1-based), not the UID. UIDs can have gaps
        // after EXPUNGE / mailboxIds-removal; sequence numbers
        // never do.
        let snapshot: Vec<(u32, u32, u64, Email)> = {
            let fix = self.fix_read();
            let acct = self.effective_account();
            mailbox_messages(&fix, acct, &selected_id)
                .into_iter()
                .enumerate()
                .filter(|(_, (uid, _))| set.matches(*uid))
                .filter_map(|(live_idx, (uid, email))| {
                    // Per-message CONDSTORE modseq drives both the
                    // emitted `MODSEQ (...)` attr and the CHANGEDSINCE
                    // filter (RFC 7162 3.1.5): keep only messages whose
                    // modseq exceeds the client's cached value. A
                    // baseline-modseq message (1, untouched since load)
                    // survives `CHANGEDSINCE 0` but not `CHANGEDSINCE
                    // 1+`; a message last touched at counter N (modseq
                    // N+1) survives any `CHANGEDSINCE < N+1`, so a
                    // mutation surfaces in the next delta FETCH.
                    let modseq = fix.email_modseq(acct, &email.id);
                    if let Some(since) = changedsince
                        && modseq <= since
                    {
                        return None;
                    }
                    let seq = u32::try_from(live_idx + 1).expect("seq fits in u32");
                    Some((seq, uid, modseq, email.clone()))
                })
                .collect()
        };

        // Per-attachment-byte sleep. Walk requested BODY[N] attrs
        // (N>=2 is the attachment-part path; N=1 is the text body
        // and not gated). For each that resolves to an actual
        // attachment on this email, sleep with the per-blob_id
        // override so a script can stage one slow attachment among
        // several fast ones.
        for (seq, uid, modseq, email) in snapshot {
            for attr in &attrs {
                if let FetchAttr::BodyPart(n) = attr
                    && *n >= 2
                    && let Some(att) = email.attachments.get((*n as usize) - 2)
                {
                    self.latency.sleep_for_attachment(&att.blob_id).await;
                }
            }
            let line = fetch_response_line(seq, uid, modseq, &email, &attrs);
            self.write_response(&line).await?;
        }
        self.write_line(&format!("{tag} OK UID FETCH completed"))
            .await
    }

    /// Like `write_line` but accepts a payload that may contain CRLFs
    /// inside an IMAP literal block. The full payload is written as
    /// one shot (no extra trailing CRLF appended by us; the caller's
    /// payload already terminates the FETCH response).
    async fn write_response(&mut self, payload: &str) -> std::io::Result<()> {
        self.writer.write_all(payload.as_bytes()).await?;
        self.writer.flush().await
    }

    /// `UID STORE <set> <flag-op> <flags>` - persistent flag
    /// writeback. Mutates the fixture's `keywords` for every matched
    /// email and bumps `Fixture::state`, so the change surfaces in
    /// the next `Email/changes` (under `updated`) and in any
    /// subsequent `UID FETCH (FLAGS)` against the same connection.
    /// Emits a `* <seq> FETCH (UID x FLAGS (...))` per matched
    /// message unless `.SILENT` was requested, then a tagged OK.
    async fn cmd_uid_store(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        // Flag writes need the `w` (write) right; `\Seen` alone would
        // technically be `s`, but the mock does not split the two.
        if self.reject_shared_write(tag, "UID STORE", "w").await? {
            return Ok(());
        }
        let (uid_set_str, after) = match split_after_set(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!(
                        "{tag} BAD UID STORE expects <set> <flag-op> <flags>"
                    ))
                    .await;
            }
        };
        let set = match parse_uid_set(uid_set_str) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID STORE bad sequence-set"))
                    .await;
            }
        };
        let store_op = match parse_store_op(after) {
            Some(op) => op,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID STORE bad flag list"))
                    .await;
            }
        };

        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");
        // Mutate under a write guard, collect the per-message FETCH
        // lines for emission *after* the guard drops. Every matched
        // message contributes one transition entry under
        // `email_updated` so a JMAP `Email/changes` against the
        // pre-mutation state reflects the writeback.
        let lines: Vec<String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let mut emitted: Vec<String> = Vec::new();
            let _ = fix.mutate(|f| {
                let mut diff = crate::fixture::MutationDiff::default();
                let mailbox_indices: Vec<usize> = f
                    .emails
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.mailbox_ids.iter().any(|id| id == &selected_id))
                    .map(|(i, _)| i)
                    .collect();
                for (slot, idx) in mailbox_indices.iter().copied().enumerate() {
                    let uid = u32::try_from(slot + 1).expect("mailbox seq fits in u32");
                    if !set.matches(uid) {
                        continue;
                    }
                    let email = &mut f.emails[idx];
                    let changed = store_op.apply_in_place(email);
                    if changed {
                        diff.email_updated.push(email.id.clone());
                    }
                    if !store_op.silent {
                        let flags = flags_for(email);
                        emitted.push(format!("* {uid} FETCH (UID {uid} FLAGS ({flags}))\r\n"));
                    }
                }
                diff
            });
            emitted
        };

        for line in lines {
            self.write_response(&line).await?;
        }
        self.write_line(&format!("{tag} OK UID STORE completed"))
            .await
    }

    /// `UID COPY <set> <mailbox>` - per RFC 3501 §6.4.8. Adds the
    /// target mailbox to each matched email's `mailbox_ids` (since
    /// the fixture model expresses an email's presence in a folder
    /// via `mailbox_ids[]`, "copy" is a set-insert; the source
    /// mailbox keeps the email and its UID). Returns `NO TRYCREATE`
    /// when the target name does not resolve to a fixture mailbox.
    /// COPYUID is omitted in v0 - ratatoskr's copy code accepts a
    /// bare OK.
    async fn cmd_uid_copy(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        // COPY inserts into the target mailbox: the `i` right.
        if self.reject_shared_write(tag, "UID COPY", "i").await? {
            return Ok(());
        }
        if !matches!(self.state, State::Selected) {
            return self
                .write_line(&format!("{tag} BAD UID COPY requires SELECT first"))
                .await;
        }
        let (uid_set_str, target_raw) = match split_after_set(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID COPY expects <set> <mailbox>"))
                    .await;
            }
        };
        let set = match parse_uid_set(uid_set_str) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID COPY bad sequence-set"))
                    .await;
            }
        };
        let target_name = match parse_one_astring(target_raw) {
            Some(n) => n,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID COPY expects \"mailbox\""))
                    .await;
            }
        };
        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");
        let outcome: Result<(), String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            // Resolve target mailbox name against the fixture-side
            // path projection (matches LIST output, case-insensitive
            // for INBOX as elsewhere).
            let target_id = list_mailboxes(&fix, &self.account_id)
                .iter()
                .find(|e| e.path.eq_ignore_ascii_case(&target_name))
                .map(|e| e.fixture_id.clone());
            match target_id {
                None => Err(format!("unknown mailbox {target_name:?}")),
                Some(target_id) => {
                    let _ = fix.mutate(|f| {
                        let mut diff = crate::fixture::MutationDiff::default();
                        // Walk uid_history for the source mailbox so
                        // UIDs match the wire view (slot N -> uid
                        // N+1, skipping retired slots).
                        let source_uids: Vec<(u32, String)> = f
                            .uid_history(&selected_id)
                            .iter()
                            .enumerate()
                            .filter_map(|(i, slot)| {
                                slot.as_ref().map(|id| {
                                    let uid = u32::try_from(i + 1).expect("uid fits in u32");
                                    (uid, id.clone())
                                })
                            })
                            .collect();
                        for (uid, email_id) in source_uids {
                            if !set.matches(uid) {
                                continue;
                            }
                            let Some(idx) = f.emails.iter().position(|e| e.id == email_id) else {
                                continue;
                            };
                            let email = &mut f.emails[idx];
                            if !email.mailbox_ids.iter().any(|id| id == &target_id) {
                                email.mailbox_ids.push(target_id.clone());
                                diff.email_updated.push(email.id.clone());
                                // Allocate a fresh UID in the
                                // target mailbox; never reused.
                                f.assign_uid(&target_id, email_id.clone());
                            }
                        }
                        diff
                    });
                    Ok(())
                }
            }
        };
        match outcome {
            Ok(()) => {
                self.write_line(&format!("{tag} OK UID COPY completed"))
                    .await
            }
            Err(reason) => {
                self.write_line(&format!("{tag} NO [TRYCREATE] {reason}"))
                    .await
            }
        }
    }

    /// `UID MOVE <set> <mailbox>` - RFC 6851 Section 3. Moves every
    /// matched message out of the selected mailbox into the target:
    /// it gains the target mailbox (with a fresh UID via
    /// `Fixture::assign_uid`) and loses the source mailbox (its source
    /// slot retired via `retire_uid`, never reused - RFC 3501 Section
    /// 2.3.1.1 UID stability), so a resync reflects the move. Emits the
    /// RFC 6851 untagged expunge report - `* VANISHED <uids>` when the
    /// connection has `ENABLE QRESYNC`d, else `* N EXPUNGE`
    /// highest-sequence-first - then a tagged `OK [COPYUID ...]`
    /// (RFC 6851 Section 4.3 / RFC 4315 Section 3).
    ///
    /// bifrost prefers `UID MOVE` whenever the server advertises the
    /// `MOVE` capability (which it now does); its `uid_move_messages`
    /// otherwise falls back to COPY + STORE `\Deleted` + UID EXPUNGE,
    /// but only when UIDPLUS is advertised. The mock advertised neither
    /// MOVE nor UIDPLUS before this, so an IMAP move could not land at
    /// all. Returns `NO [TRYCREATE]` when the target does not resolve.
    async fn cmd_uid_move(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        // MOVE is COPY + delete: insert plus delete-message.
        if self.reject_shared_write(tag, "UID MOVE", "it").await? {
            return Ok(());
        }
        if !matches!(self.state, State::Selected) {
            return self
                .write_line(&format!("{tag} BAD UID MOVE requires SELECT first"))
                .await;
        }
        let (uid_set_str, target_raw) = match split_after_set(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID MOVE expects <set> <mailbox>"))
                    .await;
            }
        };
        let set = match parse_uid_set(uid_set_str) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID MOVE bad sequence-set"))
                    .await;
            }
        };
        let target_name = match parse_one_astring(target_raw) {
            Some(n) => n,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID MOVE expects \"mailbox\""))
                    .await;
            }
        };
        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");

        // Collected for the wire report: `(src_uid, dst_uid)` pairs in
        // ascending source-uid order (COPYUID's two lists correspond
        // positionally), plus the expunged sequence numbers.
        let mut src_uids: Vec<u32> = Vec::new();
        let mut dst_uids: Vec<u32> = Vec::new();
        let mut expunged_seqs: Vec<u32> = Vec::new();

        let resolved: Result<(), String> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let target_id = list_mailboxes(&fix, &self.account_id)
                .iter()
                .find(|e| e.path.eq_ignore_ascii_case(&target_name))
                .map(|e| e.fixture_id.clone());
            match target_id {
                None => Err(format!("unknown mailbox {target_name:?}")),
                // Moving into the already-selected mailbox changes no
                // slots; accept it as a no-op rather than erroring.
                Some(target_id) if target_id == selected_id => Ok(()),
                Some(target_id) => {
                    let _ = fix.mutate(|f| {
                        let mut diff = crate::fixture::MutationDiff::default();
                        // Live (seq, uid, id) view of the source mailbox
                        // (slot index + 1 = uid; seq = live-slot ordinal).
                        let mut matched: Vec<(u32, u32, String)> = f
                            .uid_history(&selected_id)
                            .iter()
                            .enumerate()
                            .filter_map(|(i, slot)| {
                                slot.as_ref().map(|id| {
                                    let uid = u32::try_from(i + 1).expect("uid fits in u32");
                                    (uid, id.clone())
                                })
                            })
                            .enumerate()
                            .map(|(seq, (uid, id))| {
                                let seq = u32::try_from(seq + 1).expect("seq fits in u32");
                                (seq, uid, id)
                            })
                            .filter(|(_, uid, _)| set.matches(*uid))
                            .collect();
                        // Apply in ascending source-uid order so the
                        // COPYUID source / dest lists line up.
                        matched.sort_by_key(|(_, uid, _)| *uid);
                        for (seq, uid, id) in &matched {
                            let Some(idx) = f.emails.iter().position(|e| &e.id == id) else {
                                continue;
                            };
                            // Gain the target (membership + fresh UID)
                            // before dropping the source so the message
                            // is never momentarily folder-less.
                            if !f.emails[idx].mailbox_ids.iter().any(|m| m == &target_id) {
                                f.emails[idx].mailbox_ids.push(target_id.clone());
                            }
                            let dst = f.assign_uid(&target_id, id.clone());
                            f.emails[idx].mailbox_ids.retain(|m| m != &selected_id);
                            f.retire_uid(&selected_id, id);
                            src_uids.push(*uid);
                            dst_uids.push(dst);
                            expunged_seqs.push(*seq);
                            diff.email_updated.push(id.clone());
                        }
                        diff
                    });
                    Ok(())
                }
            }
        };

        if let Err(reason) = resolved {
            return self
                .write_line(&format!("{tag} NO [TRYCREATE] {reason}"))
                .await;
        }

        // Untagged expunge report (RFC 6851 Section 3): VANISHED when
        // QRESYNC is enabled (RFC 7162 Section 3.2.10), else EXPUNGE
        // highest-sequence-first so earlier (higher) removals never
        // renumber the ones still to come.
        if self.qresync_enabled {
            if !src_uids.is_empty() {
                let uids = src_uids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                self.write_line(&format!("* VANISHED {uids}")).await?;
            }
        } else {
            expunged_seqs.sort_by_key(|s| std::cmp::Reverse(*s));
            for seq in &expunged_seqs {
                self.write_line(&format!("* {seq} EXPUNGE")).await?;
            }
        }

        // Tagged OK carries COPYUID (RFC 6851 Section 4.3); UIDVALIDITY
        // is pinned at 1 across the fixture lifetime. Omit COPYUID when
        // nothing moved.
        if src_uids.is_empty() {
            self.write_line(&format!("{tag} OK UID MOVE completed"))
                .await
        } else {
            let src = src_uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let dst = dst_uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            self.write_line(&format!(
                "{tag} OK [COPYUID 1 {src} {dst}] UID MOVE completed"
            ))
            .await
        }
    }

    /// `UID EXPUNGE <set>` - per RFC 4315. Removes every matched
    /// email that carries the `\Deleted` flag from the *current*
    /// mailbox. Emits one `* <seq> EXPUNGE` per removed message,
    /// in descending sequence-number order so the wire is correct
    /// without per-line renumbering (each emission targets the
    /// then-highest-known sequence, so already-emitted entries do
    /// not shift). When the email no longer belongs to any mailbox
    /// after the operation, it is destroyed entirely; otherwise
    /// it survives in its other mailboxes.
    async fn cmd_uid_expunge(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.reject_shared_write(tag, "UID EXPUNGE", "e").await? {
            return Ok(());
        }
        if !matches!(self.state, State::Selected) {
            return self
                .write_line(&format!("{tag} BAD UID EXPUNGE requires SELECT first"))
                .await;
        }
        let set = match parse_uid_set(args.trim()) {
            Some(s) => s,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID EXPUNGE bad sequence-set"))
                    .await;
            }
        };
        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");
        let expunged_seqs: Vec<u32> = {
            let mut fix = self.fixture.write().expect("fixture lock poisoned");
            let mut seqs: Vec<u32> = Vec::new();
            let _ = fix.mutate(|f| {
                let mut diff = crate::fixture::MutationDiff::default();
                // Walk uid_history for the source mailbox; emit
                // sequence numbers from the live (non-None) slot
                // positions per RFC 3501. Each slot's uid is the
                // slot index + 1; the IMAP `* N EXPUNGE` sequence
                // number is the live-slot ordinal at the time of
                // emission (highest first to avoid renumbering
                // arithmetic).
                let live: Vec<(u32, u32, String)> = f
                    .uid_history(&selected_id)
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| {
                        slot.as_ref().map(|id| {
                            let uid = u32::try_from(i + 1).expect("uid fits in u32");
                            (uid, id.clone())
                        })
                    })
                    .enumerate()
                    .map(|(seq, (uid, id))| {
                        let seq = u32::try_from(seq + 1).expect("seq fits in u32");
                        (seq, uid, id)
                    })
                    .collect();
                let mut victims: Vec<(u32, String)> = live
                    .iter()
                    .filter(|(_seq, uid, id)| {
                        if !set.matches(*uid) {
                            return false;
                        }
                        f.emails
                            .iter()
                            .find(|e| &e.id == id)
                            .is_some_and(|e| e.keywords.iter().any(|k| k == "$deleted"))
                    })
                    .map(|(seq, _, id)| (*seq, id.clone()))
                    .collect();
                victims.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
                seqs = victims.iter().map(|(seq, _)| *seq).collect();
                for (_seq, id) in &victims {
                    let Some(idx) = f.emails.iter().position(|e| &e.id == id) else {
                        continue;
                    };
                    f.emails[idx].mailbox_ids.retain(|m| m != &selected_id);
                    f.retire_uid(&selected_id, id);
                    if f.emails[idx].mailbox_ids.is_empty() {
                        let account_id = f.emails[idx].account_id.clone();
                        f.emails.remove(idx);
                        diff.email_destroyed.push(id.clone());
                        diff.email_destroyed_accounts.push(account_id);
                    } else {
                        diff.email_updated.push(id.clone());
                    }
                }
                diff
            });
            seqs
        };

        for seq in expunged_seqs {
            self.write_line(&format!("* {seq} EXPUNGE")).await?;
        }
        self.write_line(&format!("{tag} OK UID EXPUNGE completed"))
            .await
    }

    async fn cmd_uid_search(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        let selected_id = self
            .selected
            .clone()
            .expect("Selected state requires selected mailbox");
        // Parse first (no fixture access); the BAD-criteria branch
        // returns through `.await` and we must not hold the read
        // guard across that.
        let matches = match parse_uid_search(args) {
            Some(m) => m,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID SEARCH unsupported criteria"))
                    .await;
            }
        };
        let mut hits: Vec<u32> = {
            let fix = self.fix_read();
            mailbox_messages(&fix, self.effective_account(), &selected_id)
                .iter()
                .filter(|(uid, e)| matches.matches(*uid, e))
                .map(|(uid, _)| *uid)
                .collect()
        };
        hits.sort_unstable();

        let line = if hits.is_empty() {
            "* SEARCH".to_string()
        } else {
            let mut s = String::from("* SEARCH");
            for u in &hits {
                s.push(' ');
                s.push_str(&u.to_string());
            }
            s
        };
        self.write_line(&line).await?;
        self.write_line(&format!("{tag} OK UID SEARCH completed"))
            .await
    }
}

// ── Mailbox projection ──────────────────────────────────────────────

/// IMAP wire view of a fixture mailbox.
#[derive(Debug)]
struct ListEntry {
    /// The fixture mailbox id this entry came from. Used to look up
    /// counts.
    fixture_id: String,
    /// Wire-visible path, including parent chain joined by `/`. Inbox
    /// is forced to `INBOX` (uppercase) regardless of fixture casing.
    path: String,
    /// IMAP attributes (e.g. `\Inbox`, `\Sent`). Each entry already
    /// includes the leading backslash.
    attributes: Vec<String>,
}

/// A mailbox name resolved for SELECT / MYRIGHTS / GETACL. Covers both
/// personal mailboxes (owned by the authenticated account) and shared
/// folders reached through the `#user/...` namespace.
struct ResolvedMailbox {
    /// Owning account - the one whose messages the mailbox holds.
    account_id: String,
    fixture_id: String,
    /// Canonical wire path (`INBOX`, or `#user/<owner>/<path>`).
    path: String,
    /// Rights the authenticated account holds on it.
    rights: String,
}

/// One shared folder visible to a viewer account through the
/// other-users namespace. Parallel to [`ListEntry`] but carries the
/// owning account and the viewer's rights.
struct SharedListEntry {
    fixture_id: String,
    owner_account_id: String,
    /// `#user/<owner-name>/<owner-path>` wire path.
    path: String,
    attributes: Vec<String>,
    rights: String,
}

/// Enumerate the mailboxes other accounts have shared with `viewer`,
/// projected into the `#user/<owner>/<path>` other-users namespace.
/// Driven by the fixture `[[acl]]` grants whose `identifier` is the
/// viewer; the owner segment is the owning account's `name`, and the
/// mailbox path is its own personal path (so a shared INBOX surfaces
/// as `#user/<owner>/INBOX`).
fn list_shared_mailboxes(fixture: &Fixture, viewer: &str) -> Vec<SharedListEntry> {
    let mut out = Vec::new();
    for grant in fixture.acls_for_viewer(viewer) {
        let Some(mailbox) = fixture.mailbox_by_id(&grant.mailbox_id) else {
            continue;
        };
        let owner_account_id = mailbox.account_id.clone();
        let owner_name = fixture
            .account(&owner_account_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| owner_account_id.clone());
        // The owner's personal path for this mailbox (INBOX, Work/X, ...).
        let owner_entries = list_mailboxes(fixture, &owner_account_id);
        let Some(owner_entry) = owner_entries
            .into_iter()
            .find(|e| e.fixture_id == grant.mailbox_id)
        else {
            continue;
        };
        out.push(SharedListEntry {
            fixture_id: grant.mailbox_id.clone(),
            owner_account_id,
            path: format!("{SHARED_NAMESPACE_PREFIX}{owner_name}/{}", owner_entry.path),
            attributes: owner_entry.attributes,
            rights: grant.rights.clone(),
        });
    }
    out
}

/// Whether `path` names an existing other-user mailbox, irrespective of the
/// viewer's current ACL. SELECT uses this after normal shared resolution
/// failed so a revoked grant returns the permission-shaped NOPERM response
/// instead of pretending the known mailbox never existed.
fn shared_mailbox_path_exists(fixture: &Fixture, viewer: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(SHARED_NAMESPACE_PREFIX) else {
        return false;
    };
    fixture.accounts.iter().any(|account| {
        if account.id == viewer {
            return false;
        }
        let Some(owner_path) = rest.strip_prefix(&format!("{}/", account.name)) else {
            return false;
        };
        list_mailboxes(fixture, &account.id)
            .into_iter()
            .any(|entry| entry.path.eq_ignore_ascii_case(owner_path))
    })
}

fn list_mailboxes(fixture: &Fixture, account_id: &str) -> Vec<ListEntry> {
    // The path-building walk needs to follow `parent_id` chains, but
    // parents must live in the same account (loader-enforced), so
    // scoping the id->Mailbox map by account is correct and avoids
    // a cross-account leak should a future loader relax cross-
    // account parents.
    let by_id: HashMap<&str, &Mailbox> = fixture
        .mailboxes_for(account_id)
        .map(|m| (m.id.as_str(), m))
        .collect();
    fixture
        .mailboxes_for(account_id)
        .map(|m| {
            let path = mailbox_path(m, &by_id);
            let attributes = role_attributes(m.role);
            ListEntry {
                fixture_id: m.id.clone(),
                path,
                attributes,
            }
        })
        .collect()
}

/// Split a `/`-delimited mailbox path into `(parent_path, leaf)`. A
/// single-segment path has no parent (`None`). Used by CREATE / RENAME
/// to resolve the parent mailbox and the new leaf name.
fn split_parent_leaf(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, path),
    }
}

fn mailbox_path(m: &Mailbox, by_id: &HashMap<&str, &Mailbox>) -> String {
    if m.role == Some(Role::Inbox) {
        return "INBOX".to_string();
    }
    let mut chain: Vec<&str> = vec![m.name.as_str()];
    let mut cur = m;
    while let Some(parent_id) = cur.parent_id.as_deref() {
        let Some(parent) = by_id.get(parent_id) else {
            break;
        };
        chain.push(parent.name.as_str());
        cur = parent;
    }
    chain.reverse();
    chain.join("/")
}

fn role_attributes(role: Option<Role>) -> Vec<String> {
    match role {
        Some(Role::Inbox) => vec!["\\Inbox".to_string()],
        Some(Role::Archive) => vec!["\\Archive".to_string()],
        Some(Role::Drafts) => vec!["\\Drafts".to_string()],
        Some(Role::Sent) => vec!["\\Sent".to_string()],
        Some(Role::Trash) => vec!["\\Trash".to_string()],
        Some(Role::Junk) => vec!["\\Junk".to_string()],
        Some(Role::Important) => vec!["\\Important".to_string()],
        None => vec![],
    }
}

#[derive(Debug)]
struct Counts {
    exists: u64,
    unseen: u64,
    uidnext: u64,
}

/// Yield `(uid, email)` pairs for emails in `mailbox_id`. UIDs come
/// from `Fixture::mailbox_uid_history`: the slot index plus one is
/// the IMAP UID, never reused. Slots holding `None` (the email was
/// removed from this mailbox via delete / move / expunge) are
/// skipped; their UIDs stay assigned but are no longer addressable.
/// Live email lookups go through a one-shot `id -> &Email` map so a
/// big mailbox doesn't go quadratic.
fn mailbox_messages<'a>(
    fixture: &'a Fixture,
    account_id: &'a str,
    mailbox_id: &str,
) -> Vec<(u32, &'a Email)> {
    let by_id: HashMap<&str, &Email> = fixture
        .emails_for(account_id)
        .map(|e| (e.id.as_str(), e))
        .collect();
    fixture
        .uid_history(mailbox_id)
        .iter()
        .enumerate()
        .filter_map(|(i, slot)| {
            slot.as_deref().and_then(|id| {
                by_id.get(id).map(|e| {
                    let uid = u32::try_from(i + 1).expect("uid fits in u32");
                    (uid, *e)
                })
            })
        })
        .collect()
}

/// Live IMAP UIDs for `mailbox_id`, in sequence order. A thin wrapper
/// over [`mailbox_messages`] that drops the email payload - the IDLE
/// diff only needs the UID set to detect arrivals and expunges.
fn live_uids(fixture: &Fixture, account_id: &str, mailbox_id: &str) -> Vec<u32> {
    mailbox_messages(fixture, account_id, mailbox_id)
        .iter()
        .map(|(uid, _)| *uid)
        .collect()
}

fn mailbox_counts(fixture: &Fixture, account_id: &str, mailbox_id: &str) -> Counts {
    let by_id: HashMap<&str, &Email> = fixture
        .emails_for(account_id)
        .map(|e| (e.id.as_str(), e))
        .collect();
    let mut exists: u64 = 0;
    let mut unseen: u64 = 0;
    for slot in fixture.uid_history(mailbox_id) {
        let Some(id) = slot.as_deref() else { continue };
        let Some(email) = by_id.get(id) else { continue };
        exists += 1;
        if !email.keywords.iter().any(|k| k == "$seen") {
            unseen += 1;
        }
    }
    Counts {
        exists,
        unseen,
        // UIDNEXT is monotonically increasing across the fixture's
        // lifetime: history.len() + 1 keeps growing even after a
        // delete clears a slot.
        uidnext: u64::from(fixture.uidnext(mailbox_id)),
    }
}

// ── Tiny IMAP-arg parsers ───────────────────────────────────────────

/// Parse exactly one astring, returning its raw content. Returns
/// `None` on empty input or trailing junk.
fn parse_one_astring(args: &str) -> Option<String> {
    let mut p = AstringParser { s: args, i: 0 };
    let a = p.next_astring()?;
    p.skip_spaces();
    if p.i != p.s.len() {
        return None;
    }
    Some(a)
}

/// Parse exactly two astrings (either quoted or atom), returning their
/// raw string contents. Returns `None` on syntax error.
fn parse_two_astrings(args: &str) -> Option<(String, String)> {
    let mut p = AstringParser { s: args, i: 0 };
    let a = p.next_astring()?;
    p.skip_spaces();
    let b = p.next_astring()?;
    p.skip_spaces();
    if p.i != p.s.len() {
        return None;
    }
    Some((a, b))
}

#[derive(Debug)]
struct StatusArgs {
    name: String,
    items: Vec<String>,
}

fn parse_status_args(args: &str) -> Option<StatusArgs> {
    let mut p = AstringParser { s: args, i: 0 };
    let name = p.next_astring()?;
    p.skip_spaces();
    if !p.consume('(') {
        return None;
    }
    let mut items = Vec::new();
    p.skip_spaces();
    while !p.consume(')') {
        let item = p.next_atom()?;
        items.push(item);
        p.skip_spaces();
        if p.eof() {
            return None;
        }
    }
    Some(StatusArgs { name, items })
}

// ── SASL helpers (AUTHENTICATE / account binding) ────────────────────

/// Decode a SASL response (base64-encoded, standard alphabet). The
/// IMAP client always sends standard base64 with `=` padding here -
/// not the URL-safe Gmail variant. Lenient: ignores stray padding
/// and silently drops on unrecognised chars (the SASL spec allows
/// either strict or lenient decoders).
///
/// Shared with `src/smtp.rs` (which uses the same SASL response
/// shape for `AUTH PLAIN` / `LOGIN` / `XOAUTH2` / `OAUTHBEARER`).
pub(crate) fn sasl_decode_b64(s: &str) -> Option<Vec<u8>> {
    let alpha = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut val: [i8; 256] = [-1; 256];
    for (i, &c) in alpha.iter().enumerate() {
        // alpha has 64 entries; i fits in i8.
        val[c as usize] = i8::try_from(i).expect("alpha index < 128");
    }
    let mut out: Vec<u8> = Vec::with_capacity((s.len() * 3) / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if c == b'=' || c == b'\r' || c == b'\n' || c == b' ' {
            continue;
        }
        let v = val[c as usize];
        if v < 0 {
            return None;
        }
        // v is in [0, 63] after the negativity check above.
        let v_u: u8 = u8::try_from(v).expect("non-negative base64 nibble");
        buf = (buf << 6) | u32::from(v_u);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Extract a bearer token from a SASL XOAUTH2 / OAUTHBEARER blob.
/// Both shapes use `\x01`-separated key=value pairs; we look for the
/// `auth=Bearer <token>` field. Returns None when the blob doesn't
/// carry one (e.g. SASL PLAIN, or a malformed XOAUTH2 payload).
pub(crate) fn sasl_extract_bearer(decoded: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(decoded).ok()?;
    for field in s.split('\x01') {
        if let Some(rest) = field
            .strip_prefix("auth=Bearer ")
            .or_else(|| field.strip_prefix("auth=bearer "))
        {
            let token = rest.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Extract the username from a SASL PLAIN / LOGIN / XOAUTH2 /
/// OAUTHBEARER response. PLAIN uses `\0authzid\0user\0pass` (authzid
/// is usually empty). XOAUTH2 / OAUTHBEARER prefix with `user=...`.
/// LOGIN's first round-trip is literally the username (we don't yet
/// support the second `+ Password:` round-trip; tests can extend).
/// Returns None on malformed bytes; the caller leaves the binding
/// unchanged.
pub(crate) fn sasl_extract_username(mech: &str, decoded: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(decoded).ok()?;
    match mech {
        "PLAIN" => {
            // `authzid\0user\0pass`. authzid may be empty.
            let parts: Vec<&[u8]> = decoded.split(|b| *b == 0).collect();
            if parts.len() >= 3 {
                let user = std::str::from_utf8(parts[1]).ok()?;
                if user.is_empty() {
                    None
                } else {
                    Some(user.to_string())
                }
            } else {
                None
            }
        }
        "XOAUTH2" | "OAUTHBEARER" => {
            for field in s.split('\x01') {
                if let Some(rest) = field.strip_prefix("user=") {
                    let user = rest.trim();
                    if !user.is_empty() {
                        return Some(user.to_string());
                    }
                }
                // OAUTHBEARER uses GS2 header `n,a=<user>,` for the
                // optional authorization identity. Capture it.
                if let Some(rest) = field.strip_prefix("n,a=")
                    && let Some(end) = rest.find(',')
                {
                    let user = &rest[..end];
                    if !user.is_empty() {
                        return Some(user.to_string());
                    }
                }
            }
            None
        }
        "LOGIN" => {
            // The (single) continuation in our implementation IS the
            // base64-encoded username. Drop trailing whitespace.
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// Extract the password from a decoded SASL `PLAIN` response
/// (`authzid\0user\0pass`). Only `PLAIN` carries a basic-auth password
/// in the clear; `XOAUTH2` / `OAUTHBEARER` carry bearer tokens (handled
/// separately) and `LOGIN` models only the username round-trip in v0.
/// Returns `None` for any other mechanism or a malformed response.
pub(crate) fn sasl_extract_password(mech: &str, decoded: &[u8]) -> Option<String> {
    if mech != "PLAIN" {
        return None;
    }
    let parts: Vec<&[u8]> = decoded.split(|b| *b == 0).collect();
    if parts.len() >= 3 {
        std::str::from_utf8(parts[2]).ok().map(str::to_string)
    } else {
        None
    }
}

struct AstringParser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> AstringParser<'a> {
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }
    fn skip_spaces(&mut self) {
        while self.i < self.s.len() && self.s.as_bytes()[self.i] == b' ' {
            self.i += 1;
        }
    }
    fn consume(&mut self, c: char) -> bool {
        self.skip_spaces();
        if self.s[self.i..].starts_with(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }
    /// Non-consuming lookahead: does the next non-space char equal `c`?
    fn peek(&mut self, c: char) -> bool {
        self.skip_spaces();
        self.s[self.i..].starts_with(c)
    }
    /// Skip a balanced `(...)` group starting at the cursor (which must
    /// sit on the opening paren). Returns `None` if the parens are
    /// unbalanced before end-of-input. Used to discard the QRESYNC
    /// seq-match-data the mock does not act on.
    fn skip_balanced_parens(&mut self) -> Option<()> {
        self.skip_spaces();
        if !self.consume('(') {
            return None;
        }
        let mut depth = 1usize;
        let bytes = self.s.as_bytes();
        while self.i < bytes.len() {
            match bytes[self.i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    self.i += 1;
                    if depth == 0 {
                        return Some(());
                    }
                    continue;
                }
                _ => {}
            }
            self.i += 1;
        }
        None
    }
    fn next_astring(&mut self) -> Option<String> {
        self.skip_spaces();
        if self.eof() {
            return None;
        }
        let bytes = self.s.as_bytes();
        if bytes[self.i] == b'"' {
            self.i += 1;
            let start = self.i;
            while self.i < bytes.len() && bytes[self.i] != b'"' {
                // No escape handling in v0; ratatoskr never sends
                // backslashes inside quoted folder names.
                self.i += 1;
            }
            if self.i >= bytes.len() {
                return None;
            }
            let s = self.s[start..self.i].to_string();
            self.i += 1; // closing quote
            Some(s)
        } else {
            self.next_atom()
        }
    }
    fn next_atom(&mut self) -> Option<String> {
        self.skip_spaces();
        if self.eof() {
            return None;
        }
        let bytes = self.s.as_bytes();
        let start = self.i;
        while self.i < bytes.len() {
            let b = bytes[self.i];
            if b == b' ' || b == b'(' || b == b')' || b == b'"' {
                break;
            }
            self.i += 1;
        }
        if start == self.i {
            None
        } else {
            Some(self.s[start..self.i].to_string())
        }
    }
}

/// IMAP search criteria the v0 mock understands. ratatoskr only sends
/// these three shapes (`notes/ratatoskr-imap-surface.md`); everything
/// else maps to `BAD`.
#[derive(Debug)]
enum SearchCriteria {
    All,
    UidRange { lo: u32, hi: Option<u32> },
    Since(DateTime<Utc>),
}

impl SearchCriteria {
    fn matches(&self, uid: u32, email: &Email) -> bool {
        match self {
            SearchCriteria::All => true,
            SearchCriteria::UidRange { lo, hi } => uid >= *lo && hi.is_none_or(|h| uid <= h),
            SearchCriteria::Since(ts) => email.received_at >= *ts,
        }
    }
}

fn parse_uid_search(args: &str) -> Option<SearchCriteria> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("ALL") {
        return Some(SearchCriteria::All);
    }
    // SINCE <date>
    if let Some(rest) = strip_prefix_ci(trimmed, "SINCE ") {
        let date = parse_imap_date(rest.trim())?;
        let dt = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?);
        return Some(SearchCriteria::Since(dt));
    }
    // <lo>:<hi> or <lo>:* (UID set; v0 only supports a single range).
    if let Some((lo, hi)) = trimmed.split_once(':') {
        let lo: u32 = lo.parse().ok()?;
        let hi = if hi == "*" {
            None
        } else {
            Some(hi.parse().ok()?)
        };
        return Some(SearchCriteria::UidRange { lo, hi });
    }
    None
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Parse an IMAP date string `dd-Mmm-yyyy` (RFC 3501 sec 9). The mock
/// accepts both quoted and unquoted forms.
fn parse_imap_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim_matches('"');
    NaiveDate::parse_from_str(s, "%d-%b-%Y").ok()
}

// ── UID FETCH helpers ───────────────────────────────────────────────

/// One entry in an RFC 3501 sequence-set.
#[derive(Debug, Clone, Copy)]
enum UidSetItem {
    Single(u32),
    /// `lo:hi`, where `hi == None` means `*`.
    Range(u32, Option<u32>),
}

#[derive(Debug)]
struct UidSet(Vec<UidSetItem>);

impl UidSet {
    fn matches(&self, uid: u32) -> bool {
        self.0.iter().any(|item| match item {
            UidSetItem::Single(n) => *n == uid,
            UidSetItem::Range(lo, hi) => uid >= *lo && hi.is_none_or(|h| uid <= h),
        })
    }
}

fn parse_uid_set(s: &str) -> Option<UidSet> {
    let mut items = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        if let Some((lo, hi)) = part.split_once(':') {
            let lo: u32 = lo.parse().ok()?;
            let hi = if hi == "*" {
                None
            } else {
                Some(hi.parse().ok()?)
            };
            items.push(UidSetItem::Range(lo, hi));
        } else {
            items.push(UidSetItem::Single(part.parse().ok()?));
        }
    }
    if items.is_empty() {
        None
    } else {
        Some(UidSet(items))
    }
}

/// Strip the leading sequence-set token (everything up to the first
/// space) and return `(set, rest)`.
fn split_after_set(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let (set, rest) = s.split_once(' ')?;
    Some((set.trim(), rest.trim_start()))
}

/// Parsed `STORE`/`UID STORE` flag-op argument tail.
#[derive(Debug)]
struct StoreOp {
    kind: StoreKind,
    flags: Vec<String>,
    silent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreKind {
    /// `+FLAGS`: union with existing flags.
    Add,
    /// `-FLAGS`: subtract from existing flags.
    Remove,
    /// `FLAGS`: replace existing flags entirely.
    Replace,
}

impl StoreOp {
    /// Apply the flag operation in-place to the email's `keywords`.
    /// Translates each requested IMAP flag token into its canonical
    /// fixture keyword (`\Seen` -> `$seen`, etc.) before merging so
    /// the persisted state matches what the JMAP / Graph / Gmail
    /// projections expect to see. Returns true when the mutation
    /// changed at least one keyword (caller uses the bit to decide
    /// whether to bump fixture state).
    fn apply_in_place(&self, email: &mut Email) -> bool {
        let requested: Vec<String> = self.flags.iter().map(|f| imap_flag_to_keyword(f)).collect();
        let before = email.keywords.clone();
        match self.kind {
            StoreKind::Add => {
                for k in &requested {
                    if !email.keywords.iter().any(|x| x == k) {
                        email.keywords.push(k.clone());
                    }
                }
            }
            StoreKind::Remove => {
                email.keywords.retain(|k| !requested.iter().any(|r| r == k));
            }
            StoreKind::Replace => {
                email.keywords = requested;
            }
        }
        // Stable order in the fixture so successive STOREs that
        // converge on the same set produce a byte-identical
        // projection downstream.
        email.keywords.sort();
        email.keywords.dedup();
        let mut sorted_before = before;
        sorted_before.sort();
        sorted_before.dedup();
        email.keywords != sorted_before
    }
}

/// Reverse of `flags_for`: translate an IMAP wire flag token into
/// the fixture's canonical keyword. The standard system flags
/// project to their `$`-prefixed JMAP keywords; custom tokens pass
/// through verbatim.
fn imap_flag_to_keyword(token: &str) -> String {
    match token {
        "\\Seen" => "$seen".to_string(),
        "\\Flagged" => "$flagged".to_string(),
        "\\Draft" => "$draft".to_string(),
        "\\Answered" => "$answered".to_string(),
        "\\Deleted" => "$deleted".to_string(),
        other => other.to_string(),
    }
}

fn parse_store_op(s: &str) -> Option<StoreOp> {
    let s = s.trim();
    let (op_token, rest) = s.split_once(' ')?;
    let op_upper = op_token.to_ascii_uppercase();
    let (op_word, silent) = if let Some(stripped) = op_upper.strip_suffix(".SILENT") {
        (stripped.to_string(), true)
    } else {
        (op_upper, false)
    };
    let kind = match op_word.as_str() {
        "+FLAGS" => StoreKind::Add,
        "-FLAGS" => StoreKind::Remove,
        "FLAGS" => StoreKind::Replace,
        _ => return None,
    };
    let flags = parse_flag_list(rest.trim())?;
    Some(StoreOp {
        kind,
        flags,
        silent,
    })
}

/// Parse a flag list in either `(\Seen \Flagged)` or
/// `\Seen \Flagged` shape. Returns the list of flag tokens.
fn parse_flag_list(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    let inner = if let Some(stripped) = s.strip_prefix('(') {
        stripped.strip_suffix(')')?.trim()
    } else {
        s
    };
    if inner.is_empty() {
        return Some(Vec::new());
    }
    Some(inner.split_whitespace().map(str::to_string).collect())
}

/// Split the FETCH attribute list from any trailing modifier list.
/// `(UID FLAGS) (CHANGEDSINCE 0)` -> (`(UID FLAGS)`, `(CHANGEDSINCE 0)`).
/// `(UID FLAGS)` -> (`(UID FLAGS)`, `""`).
fn split_attrs_and_modifiers(s: &str) -> (&str, &str) {
    let s = s.trim();
    if !s.starts_with('(') {
        // Bare attribute name like `UID`.
        return (s, "");
    }
    let mut depth = 0i32;
    let mut end = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        return (s, "");
    }
    let attrs = &s[..end];
    let rest = s[end..].trim_start();
    (attrs, rest)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchAttr {
    Uid,
    Flags,
    InternalDate,
    Rfc822Size,
    /// `ENVELOPE` - the RFC 3501 7.4.2 structured envelope (date,
    /// subject, from / sender / reply-to / to / cc / bcc,
    /// in-reply-to, message-id). bifrost requests this in every full
    /// FETCH projection; without it the attr list parsed to `None`
    /// and the whole `UID FETCH` replied `BAD`.
    Envelope,
    /// `MODSEQ` - RFC 4551 CONDSTORE per-message modseq. bifrost
    /// appends it whenever CONDSTORE is enabled (we advertise
    /// `CONDSTORE QRESYNC`), and rejects a value of 0.
    ModSeq,
    /// `BODY[]` or `BODY.PEEK[]`. We treat them identically because
    /// v0 does not implement flag side-effects.
    BodyFull,
    /// `BODY[HEADER]` or `.PEEK[HEADER]`.
    BodyHeader,
    /// `BODY[TEXT]` or `.PEEK[TEXT]`.
    BodyText,
    /// `BODYSTRUCTURE`. Nested IMAP body structure per RFC 3501 §7.4.2.
    BodyStructure,
    /// `BODY[N]` where N is a 1-based part number into the multipart
    /// tree. For single-part messages, only `BODY[1]` is valid (and
    /// equivalent to `BODY[TEXT]`).
    BodyPart(u32),
    /// `BODY[N.MIME]` - the MIME headers of part N (the part-level
    /// Content-Type / Content-Transfer-Encoding / Content-Disposition
    /// block) without the part body itself.
    BodyPartMime(u32),
}

fn parse_fetch_attrs(s: &str) -> Option<Vec<FetchAttr>> {
    let s = s.trim();
    let inner = if let Some(stripped) = s.strip_prefix('(') {
        stripped.strip_suffix(')')?
    } else {
        s
    };
    let mut out = Vec::new();
    for raw in inner.split_whitespace() {
        let upper = raw.to_ascii_uppercase();
        let attr = match upper.as_str() {
            "UID" => FetchAttr::Uid,
            "FLAGS" => FetchAttr::Flags,
            "INTERNALDATE" => FetchAttr::InternalDate,
            "RFC822.SIZE" => FetchAttr::Rfc822Size,
            "ENVELOPE" => FetchAttr::Envelope,
            "MODSEQ" => FetchAttr::ModSeq,
            "RFC822" | "BODY[]" | "BODY.PEEK[]" => FetchAttr::BodyFull,
            "BODY[HEADER]" | "BODY.PEEK[HEADER]" | "RFC822.HEADER" => FetchAttr::BodyHeader,
            "BODY[TEXT]" | "BODY.PEEK[TEXT]" | "RFC822.TEXT" => FetchAttr::BodyText,
            "BODYSTRUCTURE" => FetchAttr::BodyStructure,
            other => parse_part_section(other)?,
        };
        if !out.contains(&attr) {
            out.push(attr);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Stable string label for a `FetchAttr`. Used by the request-log
/// detail (`detail.attrs`) so harness scripts can match on attribute
/// names without re-parsing the raw command line.
fn fetch_attr_name(attr: &FetchAttr) -> String {
    match attr {
        FetchAttr::Uid => "UID".into(),
        FetchAttr::Flags => "FLAGS".into(),
        FetchAttr::InternalDate => "INTERNALDATE".into(),
        FetchAttr::Rfc822Size => "RFC822.SIZE".into(),
        FetchAttr::Envelope => "ENVELOPE".into(),
        FetchAttr::ModSeq => "MODSEQ".into(),
        FetchAttr::BodyFull => "BODY[]".into(),
        FetchAttr::BodyHeader => "BODY[HEADER]".into(),
        FetchAttr::BodyText => "BODY[TEXT]".into(),
        FetchAttr::BodyStructure => "BODYSTRUCTURE".into(),
        FetchAttr::BodyPart(n) => format!("BODY[{n}]"),
        FetchAttr::BodyPartMime(n) => format!("BODY[{n}.MIME]"),
    }
}

/// Parse `BODY[N]` / `BODY.PEEK[N]` / `BODY[N.MIME]` /
/// `BODY.PEEK[N.MIME]` into the matching `FetchAttr`. Caller passes
/// the upper-cased token. Returns `None` for anything else.
fn parse_part_section(token: &str) -> Option<FetchAttr> {
    let after = token
        .strip_prefix("BODY[")
        .or_else(|| token.strip_prefix("BODY.PEEK["))?;
    let inner = after.strip_suffix(']')?;
    if let Some(num) = inner.strip_suffix(".MIME") {
        let n: u32 = num.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some(FetchAttr::BodyPartMime(n))
    } else {
        let n: u32 = inner.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some(FetchAttr::BodyPart(n))
    }
}

/// Returns `Ok(Some(modseq))` if the modifiers contained
/// `CHANGEDSINCE n`, `Ok(None)` if no modifiers were present, or
/// `Err(())` on parse failure.
fn parse_changedsince(modifiers: &str) -> Result<Option<u64>, ()> {
    let m = modifiers.trim();
    if m.is_empty() {
        return Ok(None);
    }
    let inner = m
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or(())?;
    let mut it = inner.split_whitespace();
    let key = it.next().ok_or(())?;
    if !key.eq_ignore_ascii_case("CHANGEDSINCE") {
        return Err(());
    }
    let val: u64 = it.next().ok_or(())?.parse().map_err(|_| ())?;
    if it.next().is_some() {
        return Err(());
    }
    Ok(Some(val))
}

/// Parsed `SELECT`/`EXAMINE` arguments: the mailbox name plus the
/// optional RFC 7162 select-parameter list. bifrost sends
/// `SELECT INBOX (CONDSTORE)`; a QRESYNC-capable client may send
/// `SELECT INBOX (QRESYNC (<uidvalidity> <modseq> [<known-uids>]))`.
#[derive(Debug)]
struct SelectArgs {
    name: String,
    /// QRESYNC parameter when present. Carries the client's last-known
    /// view so the SELECT response can emit `VANISHED (EARLIER)`.
    qresync: Option<QResync>,
}

/// The RFC 7162 QRESYNC select-parameter contents the mock acts on.
#[derive(Debug)]
struct QResync {
    /// 3rd element (optional): the UID set the client still believes is
    /// live. When present, `VANISHED (EARLIER)` is bounded to it; when
    /// absent the mock reports every expunged slot.
    known_uids: Option<UidSet>,
}

/// Parse `SELECT`/`EXAMINE` arguments: a mailbox astring optionally
/// followed by a `(select-param ...)` group. Recognised params are
/// `CONDSTORE` and `QRESYNC (<uidvalidity> <modseq> [<known-uids>
/// [<seq-match-data>]])`. The `uidvalidity` / `modseq` / seq-match
/// elements are parsed for syntax but not acted on - the mock pins
/// UIDVALIDITY and resolves VANISHED straight from UID history - while
/// the optional known-UID set is retained to bound VANISHED output.
/// Returns `None` on any syntax error so the caller replies `BAD`.
fn parse_select_args(args: &str) -> Option<SelectArgs> {
    let mut p = AstringParser { s: args, i: 0 };
    let name = p.next_astring()?;
    p.skip_spaces();
    let mut out = SelectArgs {
        name,
        qresync: None,
    };
    if p.eof() {
        return Some(out);
    }
    if !p.consume('(') {
        return None;
    }
    while !p.consume(')') {
        let key = p.next_atom()?;
        if key.eq_ignore_ascii_case("CONDSTORE") {
            // CONDSTORE-only: HIGHESTMODSEQ is always reported, so the
            // flag needs no state beyond being accepted.
        } else if key.eq_ignore_ascii_case("QRESYNC") {
            if !p.consume('(') {
                return None;
            }
            // uidvalidity + modseq: required, parsed for syntax only.
            let _uidvalidity: u32 = p.next_atom()?.parse().ok()?;
            let _modseq: u64 = p.next_atom()?.parse().ok()?;
            // Optional known-UID set (3rd element). Absent when the next
            // token closes the QRESYNC group or opens the seq-match
            // parens.
            let known_uids = if p.peek(')') || p.peek('(') {
                None
            } else {
                Some(parse_uid_set(&p.next_atom()?)?)
            };
            // Optional 4th element: seq-match-data `(known-seqs
            // known-uids)`. Parsed-and-skipped: the mock derives
            // VANISHED from its own UID history, so the client's
            // seq<->uid map is moot.
            if p.peek('(') {
                p.skip_balanced_parens()?;
            }
            if !p.consume(')') {
                return None;
            }
            out.qresync = Some(QResync { known_uids });
        } else {
            // Unknown select-param: reject rather than silently mislead.
            return None;
        }
        p.skip_spaces();
        if p.eof() {
            return None;
        }
    }
    p.skip_spaces();
    if !p.eof() {
        return None;
    }
    Some(out)
}

/// UIDs expunged from `mailbox_id` (slots retired to `None` in the UID
/// history), bounded to `known` when the client supplied a QRESYNC
/// known-UID set. Ascending. Drives `VANISHED (EARLIER)`.
fn expunged_uids(fixture: &Fixture, mailbox_id: &str, known: Option<&UidSet>) -> Vec<u32> {
    fixture
        .uid_history(mailbox_id)
        .iter()
        .enumerate()
        .filter_map(|(i, slot)| {
            if slot.is_some() {
                return None;
            }
            let uid = u32::try_from(i + 1).expect("uid fits in u32");
            if known.is_none_or(|k| k.matches(uid)) {
                Some(uid)
            } else {
                None
            }
        })
        .collect()
}

/// Compress an ascending UID list into an IMAP sequence-set string,
/// collapsing runs into `lo:hi` and joining with commas
/// (`[1,2,3,7] -> "1:3,7"`). Input must be sorted ascending and
/// non-empty.
fn format_uid_ranges(uids: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < uids.len() {
        let lo = uids[i];
        let mut hi = lo;
        while i + 1 < uids.len() && uids[i + 1] == hi + 1 {
            hi = uids[i + 1];
            i += 1;
        }
        if lo == hi {
            parts.push(lo.to_string());
        } else {
            parts.push(format!("{lo}:{hi}"));
        }
        i += 1;
    }
    parts.join(",")
}

/// Render a single `* <seq> FETCH (...)` response line. The returned
/// Pre-computed RFC 822 projection of an email, lazily populated.
/// Pre-fix `RFC822.SIZE` + `BODY[]` + `BODY[HEADER]` + `BODY[TEXT]`
/// on one fetch line called `render_rfc822*` four times; `split_raw`
/// re-scanned the raw block per attr. With this cache each fetch
/// renders once even for raw-bytes emails (one `split_raw`) and
/// even for huge multipart bodies.
struct RenderedRfc822 {
    headers: String,
    text: String,
    full: String,
}

impl RenderedRfc822 {
    fn for_email(email: &Email) -> Self {
        if let Some(raw) = &email.raw_bytes {
            let (h, t) = split_raw(raw);
            return Self {
                headers: h.to_string(),
                text: t.to_string(),
                full: raw.clone(),
            };
        }
        let headers = render_rfc822_headers(email);
        let text = render_rfc822_text_body(email);
        let mut full = String::with_capacity(headers.len() + 2 + text.len());
        full.push_str(&headers);
        full.push_str("\r\n");
        full.push_str(&text);
        Self {
            headers,
            text,
            full,
        }
    }
}

/// The full assembled RFC 822 message for `email` - headers + body,
/// multipart/mixed when it carries attachments, or the verbatim
/// `raw_bytes` when the fixture set them. Mirrors what `UID FETCH
/// BODY[]` emits, and is shared with the Graph `$value` body-fetch
/// path so the two surfaces agree byte-for-byte.
pub(crate) fn assembled_rfc822(email: &Email) -> String {
    RenderedRfc822::for_email(email).full
}

/// string already terminates with `\r\n` and may contain CRLFs inside
/// an IMAP literal block.
fn fetch_response_line(
    seq: u32,
    uid: u32,
    modseq: u64,
    email: &Email,
    attrs: &[FetchAttr],
) -> String {
    let mut out = format!("* {seq} FETCH (");
    let mut first = true;
    // Lazy: render only if a body-shaped attr asks for it.
    let mut rendered: Option<RenderedRfc822> = None;
    for attr in attrs {
        if !first {
            out.push(' ');
        }
        first = false;
        match attr {
            FetchAttr::Uid => {
                out.push_str(&format!("UID {uid}"));
            }
            FetchAttr::Flags => {
                out.push_str(&format!("FLAGS ({})", flags_for(email)));
            }
            FetchAttr::InternalDate => {
                out.push_str(&format!(
                    "INTERNALDATE \"{}\"",
                    email.received_at.format("%d-%b-%Y %H:%M:%S %z")
                ));
            }
            FetchAttr::Rfc822Size => {
                let r = rendered.get_or_insert_with(|| RenderedRfc822::for_email(email));
                out.push_str(&format!("RFC822.SIZE {}", r.full.len()));
            }
            FetchAttr::Envelope => {
                out.push_str(&format!("ENVELOPE {}", render_envelope(email)));
            }
            FetchAttr::ModSeq => {
                // CONDSTORE per-message modseq: the change counter of
                // this email's last create/update, plus one (see
                // `Fixture::email_modseq`). Always >= 1 (bifrost rejects
                // 0); a baseline message reports `MODSEQ (1)`, a mutated
                // one reports its higher modseq so bifrost's CHANGEDSINCE
                // cache advances.
                out.push_str(&format!("MODSEQ ({modseq})"));
            }
            FetchAttr::BodyFull => {
                let r = rendered.get_or_insert_with(|| RenderedRfc822::for_email(email));
                out.push_str(&format!("BODY[] {{{}}}\r\n{}", r.full.len(), r.full));
            }
            FetchAttr::BodyHeader => {
                let r = rendered.get_or_insert_with(|| RenderedRfc822::for_email(email));
                out.push_str(&format!(
                    "BODY[HEADER] {{{}}}\r\n{}",
                    r.headers.len(),
                    r.headers
                ));
            }
            FetchAttr::BodyText => {
                let r = rendered.get_or_insert_with(|| RenderedRfc822::for_email(email));
                out.push_str(&format!("BODY[TEXT] {{{}}}\r\n{}", r.text.len(), r.text));
            }
            FetchAttr::BodyStructure => {
                out.push_str(&format!("BODYSTRUCTURE {}", render_bodystructure(email)));
            }
            FetchAttr::BodyPart(n) => match render_part_n(email, *n) {
                Some(bytes) => out.push_str(&format!("BODY[{n}] {{{}}}\r\n{bytes}", bytes.len())),
                None => out.push_str(&format!("BODY[{n}] NIL")),
            },
            FetchAttr::BodyPartMime(n) => match render_part_n_mime(email, *n) {
                Some(bytes) => {
                    out.push_str(&format!("BODY[{n}.MIME] {{{}}}\r\n{bytes}", bytes.len()));
                }
                None => out.push_str(&format!("BODY[{n}.MIME] NIL")),
            },
        }
    }
    out.push_str(")\r\n");
    out
}

/// Map fixture keywords to IMAP flag tokens. `$seen` -> `\Seen`,
/// `$flagged` -> `\Flagged`. Anything else is a custom keyword and
/// passes through verbatim.
fn flags_for(email: &Email) -> String {
    let mut tokens: Vec<String> = email
        .keywords
        .iter()
        .map(|k| match k.as_str() {
            "$seen" => "\\Seen".to_string(),
            "$flagged" => "\\Flagged".to_string(),
            "$draft" => "\\Draft".to_string(),
            "$answered" => "\\Answered".to_string(),
            "$deleted" => "\\Deleted".to_string(),
            other => other.to_string(),
        })
        .collect();
    tokens.sort();
    tokens.join(" ")
}

// ── ENVELOPE emission (RFC 3501 7.4.2) ──────────────────────────────
//
// `ENVELOPE (date subject from sender reply-to to cc bcc in-reply-to
// message-id)`. Address structures are `(name adl mailbox host)`;
// we always emit NIL for the (source-route) adl field. Per RFC 3501,
// when the message has no Sender / Reply-To the server defaults both
// to the From value - bifrost relies on that to resolve a sender.
// v0 fixtures are ASCII, so quoted strings (no RFC 2047 / literals)
// suffice; we escape `"` and `\` for safety.

fn render_envelope(email: &Email) -> String {
    let date = imap_nstring(Some(&email.sent_at.to_rfc2822()));
    let subject = imap_nstring(email.subject.as_deref());
    let from = imap_addr_list(email.from.iter());
    // Sender + Reply-To default to From when the message carries
    // neither (RFC 3501 7.4.2).
    let sender = from.clone();
    let reply_to = if email.reply_to.is_empty() {
        from.clone()
    } else {
        imap_addr_list(email.reply_to.iter())
    };
    let to = imap_addr_list(email.to.iter());
    let cc = imap_addr_list(email.cc.iter());
    let bcc = imap_addr_list(email.bcc.iter());
    let in_reply_to = imap_nstring_join(&email.in_reply_to);
    let message_id = imap_nstring_join(&email.message_id);
    format!(
        "({date} {subject} {from} {sender} {reply_to} {to} {cc} {bcc} {in_reply_to} {message_id})"
    )
}

/// IMAP `nstring`: a quoted string, or the bare atom `NIL` for the
/// absent value. Escapes `"` and `\` per RFC 3501 quoted-string
/// rules.
fn imap_nstring(s: Option<&str>) -> String {
    match s {
        None => "NIL".to_string(),
        Some(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

/// Join a multi-value header (`References`-style) into a single
/// nstring, or `NIL` when empty. ENVELOPE carries `in-reply-to` and
/// `message-id` as single strings.
fn imap_nstring_join(parts: &[String]) -> String {
    if parts.is_empty() {
        "NIL".to_string()
    } else {
        imap_nstring(Some(&parts.join(" ")))
    }
}

/// An ENVELOPE address list: `NIL` when empty, else a parenthesised
/// run of address structures with no separators between them.
fn imap_addr_list<'a>(addrs: impl Iterator<Item = &'a Address>) -> String {
    let mut peekable = addrs.peekable();
    if peekable.peek().is_none() {
        return "NIL".to_string();
    }
    let mut out = String::from("(");
    for a in peekable {
        out.push_str(&imap_addr(a));
    }
    out.push(')');
    out
}

/// One ENVELOPE address structure `(name adl mailbox host)`. `adl`
/// (source route) is always NIL. The address splits on the last `@`
/// into mailbox + host; an address with no `@` keeps the whole thing
/// as the mailbox and emits NIL host.
fn imap_addr(a: &Address) -> String {
    let name = imap_nstring(a.name.as_deref());
    let (mailbox, host) = match a.email.rsplit_once('@') {
        Some((m, h)) => (imap_nstring(Some(m)), imap_nstring(Some(h))),
        None => (imap_nstring(Some(&a.email)), "NIL".to_string()),
    };
    format!("({name} NIL {mailbox} {host})")
}

// ── RFC 822 emission ────────────────────────────────────────────────
//
// Hand-rolled to keep the dep surface small. v0
// fixtures are ASCII-only, so we don't need RFC 2047 encoded-words or
// header line folding. When fixtures grow non-ASCII subjects or
// multipart bodies, this is the moment to swap in `mail-builder`.

// `render_rfc822` removed: callers now go through
// `RenderedRfc822::for_email`, which composes headers + text once
// and exposes both pieces alongside the full block. Unit tests
// in this module that needed the legacy entry point were updated
// to call `RenderedRfc822::for_email(email).full` instead.

/// For raw-bytes emails, slice the verbatim block at the first
/// `\r\n\r\n` to recover header / body sub-fetches. The header slice
/// stops at `i + 2` so it includes the last header field's
/// terminating CRLF but not the blank-line CRLF (matches what the
/// structured `render_rfc822_headers` returns - that function emits
/// "Header: value\r\n" lines and the caller appends the blank-line
/// CRLF separately). The body slice starts at `i + 4`, after the
/// blank line. If no terminator is present (a fixture deliberately
/// authoring a malformed message with no header/body separator), the
/// whole block is treated as headers and the text body comes back
/// empty.
fn split_raw(raw: &str) -> (&str, &str) {
    match raw.find("\r\n\r\n") {
        Some(i) => (&raw[..i + 2], &raw[i + 4..]),
        None => (raw, ""),
    }
}

fn render_rfc822_headers(email: &Email) -> String {
    if let Some(raw) = &email.raw_bytes {
        return split_raw(raw).0.to_string();
    }
    let mut out = String::new();
    if let Some(from) = &email.from {
        push_header(&mut out, "From", &format_address(from));
    }
    if !email.to.is_empty() {
        push_header(&mut out, "To", &format_address_list(&email.to));
    }
    if !email.cc.is_empty() {
        push_header(&mut out, "Cc", &format_address_list(&email.cc));
    }
    if !email.bcc.is_empty() {
        push_header(&mut out, "Bcc", &format_address_list(&email.bcc));
    }
    if !email.reply_to.is_empty() {
        push_header(&mut out, "Reply-To", &format_address_list(&email.reply_to));
    }
    push_header(&mut out, "Date", &email.sent_at.to_rfc2822());
    if let Some(subject) = &email.subject {
        push_header(&mut out, "Subject", subject);
    }
    if !email.message_id.is_empty() {
        push_header(&mut out, "Message-ID", &email.message_id.join(" "));
    }
    if !email.in_reply_to.is_empty() {
        push_header(&mut out, "In-Reply-To", &email.in_reply_to.join(" "));
    }
    if !email.references.is_empty() {
        push_header(&mut out, "References", &email.references.join(" "));
    }
    push_header(&mut out, "MIME-Version", "1.0");
    if email.attachments.is_empty() {
        push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
        push_header(&mut out, "Content-Transfer-Encoding", "8bit");
    } else {
        push_header(
            &mut out,
            "Content-Type",
            &format!(
                "multipart/mixed; boundary=\"{}\"",
                multipart_boundary(email)
            ),
        );
    }
    out
}

fn render_rfc822_text_body(email: &Email) -> String {
    if let Some(raw) = &email.raw_bytes {
        return split_raw(raw).1.to_string();
    }
    if email.attachments.is_empty() {
        return match &email.body {
            Body::Text(t) => normalize_crlf(t),
        };
    }
    render_multipart_body(email)
}

fn multipart_boundary(email: &Email) -> String {
    format!("=_saehrimnir_{}_=", email.id)
}

fn render_multipart_body(email: &Email) -> String {
    let boundary = multipart_boundary(email);
    let mut out = String::new();
    // Part 1: text body.
    out.push_str(&format!("--{boundary}\r\n"));
    out.push_str(&part_mime_text());
    out.push_str(&match &email.body {
        Body::Text(t) => normalize_crlf(t),
    });
    out.push_str("\r\n");
    // Part 2..N: attachments.
    for a in &email.attachments {
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str(&part_mime_attachment(a));
        out.push_str(&base64_wrapped(&a.data));
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    out
}

fn part_mime_text() -> String {
    let mut out = String::new();
    push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
    push_header(&mut out, "Content-Transfer-Encoding", "8bit");
    out.push_str("\r\n");
    out
}

fn part_mime_attachment(a: &crate::fixture::Attachment) -> String {
    let mut out = String::new();
    push_header(
        &mut out,
        "Content-Type",
        &format!("{}; name=\"{}\"", a.content_type, a.name),
    );
    push_header(
        &mut out,
        "Content-Disposition",
        &format!("{}; filename=\"{}\"", a.disposition.as_str(), a.name),
    );
    push_header(&mut out, "Content-Transfer-Encoding", "base64");
    if let Some(cid) = &a.cid {
        push_header(&mut out, "Content-ID", &format!("<{cid}>"));
    }
    out.push_str("\r\n");
    out
}

/// Standard base64 (with padding), wrapped at 76 chars per line, CRLF
/// separated. Final line gets a trailing CRLF so the next boundary sits
/// on its own line.
fn base64_wrapped(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, rem) = input.as_chunks::<3>();
    for &[c0, c1, c2] in chunks {
        let n = (u32::from(c0) << 16) | (u32::from(c1) << 8) | u32::from(c2);
        encoded.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        encoded.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        encoded.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            encoded.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            encoded.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            encoded.push_str("==");
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            encoded.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            encoded.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            encoded.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            encoded.push('=');
        }
        _ => {}
    }
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    let mut i = 0;
    while i < encoded.len() {
        let end = (i + 76).min(encoded.len());
        out.push_str(&encoded[i..end]);
        out.push_str("\r\n");
        i = end;
    }
    out
}

/// Render IMAP `BODYSTRUCTURE` for an email. Single-part text and
/// multipart/mixed are the only shapes v0 emits. Raw-bytes emails
/// project as a single text/plain leaf reporting the raw octet
/// count + line count - this is a deliberate lie for adversarial-
/// shape fixtures (the bytes might be malformed multipart that the
/// mock cannot honestly summarise), but it gives a syntactically
/// valid response a non-parsing client can ignore.
fn render_bodystructure(email: &Email) -> String {
    if let Some(raw) = &email.raw_bytes {
        return body_structure_text_leaf(raw, "8BIT", &[("CHARSET", "utf-8")]);
    }
    if email.attachments.is_empty() {
        let body = match &email.body {
            Body::Text(t) => normalize_crlf(t),
        };
        return body_structure_text_leaf(&body, "8BIT", &[("CHARSET", "utf-8")]);
    }
    let mut parts = String::new();
    let body = match &email.body {
        Body::Text(t) => normalize_crlf(t),
    };
    parts.push_str(&body_structure_text_leaf(
        &body,
        "8BIT",
        &[("CHARSET", "utf-8")],
    ));
    for a in &email.attachments {
        parts.push_str(&body_structure_attachment_leaf(a));
    }
    format!(
        "({parts} \"MIXED\" (\"BOUNDARY\" \"{}\") NIL NIL NIL)",
        multipart_boundary(email)
    )
}

fn body_structure_text_leaf(body: &str, encoding: &str, params: &[(&str, &str)]) -> String {
    let octets = body.len();
    let lines =
        body.matches("\r\n").count() + usize::from(!body.is_empty() && !body.ends_with("\r\n"));
    format!(
        "(\"TEXT\" \"PLAIN\" {} NIL NIL \"{encoding}\" {octets} {lines})",
        format_params(params)
    )
}

fn body_structure_attachment_leaf(a: &crate::fixture::Attachment) -> String {
    let (typ, sub) = split_media_type(&a.content_type);
    let encoded = base64_wrapped(&a.data);
    let octets = encoded.len();
    let params = [("NAME", a.name.as_str())];
    if typ.eq_ignore_ascii_case("text") {
        let lines = encoded.matches("\r\n").count();
        format!(
            "(\"{}\" \"{}\" {} NIL NIL \"BASE64\" {octets} {lines})",
            typ.to_uppercase(),
            sub.to_uppercase(),
            format_params(&params)
        )
    } else {
        format!(
            "(\"{}\" \"{}\" {} NIL NIL \"BASE64\" {octets})",
            typ.to_uppercase(),
            sub.to_uppercase(),
            format_params(&params)
        )
    }
}

fn split_media_type(ct: &str) -> (&str, &str) {
    let main = ct.split(';').next().unwrap_or(ct).trim();
    match main.split_once('/') {
        Some((t, s)) => (t, s),
        None => (main, "PLAIN"),
    }
}

fn format_params(params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return "NIL".to_string();
    }
    let mut out = String::from("(");
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("\"{}\" \"{v}\"", k.to_uppercase()));
    }
    out.push(')');
    out
}

/// Return the wire bytes for `BODY[N]`. Part 1 is the text body, parts
/// 2..N+1 are the attachments. For single-part messages, only N=1 is
/// valid (and equals the body text). Returns `None` for out-of-range
/// part numbers. Raw-bytes emails answer N=1 with the post-header
/// slice (matching what `BODY[TEXT]` returns) and `None` for any
/// other N - the mock does not parse the raw block to discover sub-
/// parts.
fn render_part_n(email: &Email, n: u32) -> Option<String> {
    if n == 0 {
        return None;
    }
    if let Some(raw) = &email.raw_bytes {
        if n == 1 {
            return Some(split_raw(raw).1.to_string());
        }
        return None;
    }
    if n == 1 {
        return Some(match &email.body {
            Body::Text(t) => normalize_crlf(t),
        });
    }
    let idx = (n as usize) - 2;
    let att = email.attachments.get(idx)?;
    Some(base64_wrapped(&att.data))
}

/// Return the MIME header block for `BODY[N.MIME]` (Content-Type +
/// Content-Transfer-Encoding + optional Content-Disposition / -ID,
/// terminated by a blank line). For single-part messages only N=1 is
/// valid; the MIME headers there are the message-level Content-Type
/// and -Encoding pair.
fn render_part_n_mime(email: &Email, n: u32) -> Option<String> {
    if n == 0 {
        return None;
    }
    if let Some(raw) = &email.raw_bytes {
        if n == 1 {
            return Some(split_raw(raw).0.to_string());
        }
        return None;
    }
    if email.attachments.is_empty() {
        if n == 1 {
            return Some(part_mime_text());
        }
        return None;
    }
    if n == 1 {
        return Some(part_mime_text());
    }
    let idx = (n as usize) - 2;
    let att = email.attachments.get(idx)?;
    Some(part_mime_attachment(att))
}

fn push_header(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
    out.push_str("\r\n");
}

fn format_address(a: &Address) -> String {
    match &a.name {
        Some(name) => format!("{name} <{}>", a.email),
        None => format!("<{}>", a.email),
    }
}

fn format_address_list(xs: &[Address]) -> String {
    xs.iter().map(format_address).collect::<Vec<_>>().join(", ")
}

/// Force every `\n` to `\r\n` so the RFC 822 wire body has consistent
/// line endings, which matters for the literal-block byte count.
fn normalize_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// Match an IMAP LIST pattern. v0 supports `*` (match anything) and
/// exact strings; the `%` non-hierarchy wildcard is not implemented
/// because ratatoskr never sends it.
fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern == candidate {
        return true;
    }
    // Allow trailing `*` and leading `*` so things like `Inbox*`
    // continue to work on whatever the client decides to throw.
    if let Some(prefix) = pattern.strip_suffix('*')
        && candidate.starts_with(prefix)
    {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*')
        && candidate.ends_with(suffix)
    {
        return true;
    }
    false
}

fn strip_crlf(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

#[derive(Debug)]
struct Parsed<'a> {
    tag: &'a str,
    command: &'a str,
    args: &'a str,
}

/// Split a command line into tag / command / rest. Returns `None` if
/// the line is empty or has no command word.
fn parse_command_line(line: &str) -> Option<Parsed<'_>> {
    let line = line.trim_end_matches(' ');
    if line.is_empty() {
        return None;
    }
    let (tag, rest) = line.split_once(' ')?;
    let rest = rest.trim_start_matches(' ');
    if rest.is_empty() {
        return None;
    }
    let (command, args) = match rest.split_once(' ') {
        Some((c, a)) => (c, a.trim_start_matches(' ')),
        None => (rest, ""),
    };
    Some(Parsed { tag, command, args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Account, Fixture};
    use chrono::TimeZone;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fixture() -> crate::shared::FixtureHandle {
        crate::shared::handle(Fixture {
            name: "t".into(),
            state_seed: "s1".into(),
            accounts: vec![Account {
                id: "a".into(),
                name: "a@b".into(),
                is_personal: true,
                primary: true,
            }],
            mailboxes: vec![],
            emails: vec![],
            oauth: crate::fixture::OAuthConfig::default(),
            discovery: crate::fixture::DiscoveryConfig::default(),
            calendars: vec![],
            events: vec![],
            account_logs: Default::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            contact_groups: vec![],
            other_contacts: vec![],
            directory_people: vec![],
            categories: vec![],
            groups: vec![],
            acls: vec![],
            public_folders: vec![],
            public_items: vec![],
            send_as: vec![],
            mailbox_uid_history: HashMap::new(),
            synthetic_event_seq: 0,
            synthetic_email_seq: 0,
            synthetic_category_seq: 0,
            synthetic_contact_seq: 0,
            uploaded_blobs: std::collections::BTreeMap::new(),
            gmail_label_colors: std::collections::BTreeMap::new(),
        })
    }

    fn fixture_with_folders() -> crate::shared::FixtureHandle {
        use crate::fixture::{Body, Email, Mailbox};
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let mk_email = |id: &str, mailbox: &str, seen: bool| Email {
            id: id.into(),
            account_id: "a".into(),
            thread_id: format!("t-{id}"),
            mailbox_ids: vec![mailbox.into()],
            keywords: if seen { vec!["$seen".into()] } else { vec![] },
            size: 1,
            received_at: ts,
            sent_at: ts,
            from: None,
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            subject: None,
            preview: None,
            message_id: vec![],
            in_reply_to: vec![],
            references: vec![],
            has_attachment: false,
            body: Body::Text("x".into()),
            attachments: vec![],
            raw_bytes: None,
        };
        let mut fix = Fixture {
            name: "f".into(),
            state_seed: "s1".into(),
            accounts: vec![Account {
                id: "a".into(),
                name: "a@b".into(),
                is_personal: true,
                primary: true,
            }],
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    account_id: "a".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    account_id: "a".into(),
                    name: "Archive".into(),
                    role: Some(Role::Archive),
                    parent_id: None,
                    sort_order: Some(1),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-projects".into(),
                    account_id: "a".into(),
                    name: "Projects".into(),
                    role: None,
                    parent_id: None,
                    sort_order: Some(2),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-rust".into(),
                    account_id: "a".into(),
                    name: "Rust".into(),
                    role: None,
                    parent_id: Some("mb-projects".into()),
                    sort_order: Some(3),
                    is_subscribed: true,
                },
            ],
            emails: vec![
                mk_email("e1", "mb-inbox", true),
                mk_email("e2", "mb-inbox", false),
                mk_email("e3", "mb-archive", true),
            ],
            oauth: crate::fixture::OAuthConfig::default(),
            discovery: crate::fixture::DiscoveryConfig::default(),
            calendars: vec![],
            events: vec![],
            account_logs: Default::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            contact_groups: vec![],
            other_contacts: vec![],
            directory_people: vec![],
            categories: vec![],
            groups: vec![],
            acls: vec![],
            public_folders: vec![],
            public_items: vec![],
            send_as: vec![],
            mailbox_uid_history: HashMap::new(),
            synthetic_event_seq: 0,
            synthetic_email_seq: 0,
            synthetic_category_seq: 0,
            synthetic_contact_seq: 0,
            uploaded_blobs: std::collections::BTreeMap::new(),
            gmail_label_colors: std::collections::BTreeMap::new(),
        };
        // Test fixture - rebuild as if loaded.
        fix.rebuild_uid_history();
        crate::shared::handle(fix)
    }

    #[test]
    fn parses_simple_command() {
        let p = parse_command_line("a1 CAPABILITY").unwrap();
        assert_eq!(p.tag, "a1");
        assert_eq!(p.command, "CAPABILITY");
        assert_eq!(p.args, "");
    }

    #[test]
    fn parses_command_with_args() {
        let p = parse_command_line("tag2 LOGIN \"user\" \"pass\"").unwrap();
        assert_eq!(p.tag, "tag2");
        assert_eq!(p.command, "LOGIN");
        assert_eq!(p.args, "\"user\" \"pass\"");
    }

    #[test]
    fn empty_or_tagless_returns_none() {
        assert!(parse_command_line("").is_none());
        assert!(parse_command_line("   ").is_none());
        assert!(parse_command_line("onlytag").is_none());
    }

    /// Drive a connection over a duplex stream. Returns the bytes the
    /// server emitted in response to `script`, after closing the client
    /// half so the server's read loop terminates.
    async fn run_script(script: &[u8]) -> String {
        run_script_with(script, fixture()).await
    }

    async fn run_script_with(script: &[u8], fix: crate::shared::FixtureHandle) -> String {
        let (server, mut client) = tokio::io::duplex(8192);
        let (_tx, rx) = watch::channel(false);
        let server_task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(
                server,
                fix,
                None,
                crate::oauth::TokenStore::default(),
                crate::request_log::RequestLog::default(),
                crate::latency::LatencyKnob::default(),
                crate::push::PushHub::new(),
                &mut rx,
            )
            .await
        });

        client.write_all(script).await.unwrap();
        client.shutdown().await.unwrap();

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        server_task.await.unwrap().unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn greeting_emitted_immediately() {
        let out = run_script(b"").await;
        assert_eq!(out, GREETING);
    }

    #[tokio::test]
    async fn capability_returns_advertised_set() {
        let out = run_script(b"a1 CAPABILITY\r\n").await;
        assert!(out.contains(GREETING));
        assert!(
            out.contains(
                "* CAPABILITY IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL\r\n"
            ),
            "got: {out:?}"
        );
        assert!(out.contains("a1 OK CAPABILITY completed\r\n"));
        for k in ["STARTTLS", "COMPRESS", "NOTIFY", "APPEND"] {
            assert!(
                !out.contains(k),
                "advertised banned capability {k}: {out:?}"
            );
        }
    }

    #[tokio::test]
    async fn noop_acks_with_tag() {
        let out = run_script(b"x NOOP\r\n").await;
        assert!(out.contains("x OK NOOP completed\r\n"));
    }

    #[tokio::test]
    async fn logout_emits_bye_and_closes() {
        let out = run_script(b"q LOGOUT\r\n").await;
        assert!(out.contains("* BYE saehrimnir signing off\r\n"));
        assert!(out.contains("q OK LOGOUT completed\r\n"));
    }

    #[tokio::test]
    async fn logout_truncates_subsequent_commands() {
        let out = run_script(b"q LOGOUT\r\nx2 NOOP\r\n").await;
        assert!(out.contains("q OK LOGOUT completed\r\n"));
        assert!(!out.contains("x2 OK"));
    }

    #[tokio::test]
    async fn unknown_command_is_bad_not_close() {
        let out = run_script(b"a APPEND inbox\r\nb NOOP\r\n").await;
        assert!(out.contains("a BAD APPEND not implemented"));
        assert!(out.contains("b OK NOOP completed"));
    }

    #[tokio::test]
    async fn case_insensitive_command_matching() {
        let out = run_script(b"a1 capability\r\n").await;
        assert!(out.contains("a1 OK CAPABILITY completed"));
    }

    // ── Auth tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn login_accepts_any_credential() {
        let out = run_script(b"a LOGIN \"alice\" \"hunter2\"\r\n").await;
        assert!(
            out.contains(
                "a OK [CAPABILITY IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL] LOGIN completed"
            ),
            "got: {out:?}"
        );
    }

    #[tokio::test]
    async fn login_rejected_after_already_authenticated() {
        let out = run_script(b"a LOGIN \"u\" \"p\"\r\nb LOGIN \"u\" \"p\"\r\n").await;
        assert!(out.contains("a OK"));
        assert!(out.contains("b BAD LOGIN only valid pre-auth"));
    }

    #[tokio::test]
    async fn authenticate_plain_with_initial_response() {
        // SASL-IR path: client sends the encoded credential on the
        // same line, so the server should not prompt.
        let out = run_script(b"a AUTHENTICATE PLAIN AGFsaWNlAGh1bnRlcg==\r\n").await;
        assert!(
            out.contains(
                "a OK [CAPABILITY IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL] PLAIN authentication accepted"
            ),
            "got: {out:?}"
        );
        // No `+` prompt expected.
        assert!(!out.contains("\r\n+\r\n") && !out.starts_with("+\r\n"));
    }

    #[tokio::test]
    async fn authenticate_plain_with_continuation() {
        // Client uses the two-step flow: AUTHENTICATE alone, then sends
        // the base64 in a follow-up line.
        let out = run_script(b"a AUTHENTICATE PLAIN\r\nAGFsaWNlAGh1bnRlcg==\r\n").await;
        assert!(
            out.contains("+\r\n"),
            "expected continuation prompt: {out:?}"
        );
        assert!(out.contains("a OK") && out.contains("PLAIN authentication accepted"));
    }

    #[tokio::test]
    async fn authenticate_can_be_aborted_with_star() {
        let out = run_script(b"a AUTHENTICATE PLAIN\r\n*\r\n").await;
        assert!(out.contains("a BAD AUTHENTICATE aborted"));
    }

    #[tokio::test]
    async fn authenticate_unsupported_mechanism_returns_no() {
        let out = run_script(b"a AUTHENTICATE GSSAPI\r\n").await;
        assert!(out.contains("a NO unsupported SASL mechanism"));
    }

    #[tokio::test]
    async fn authenticate_xoauth2_and_oauthbearer_accepted() {
        let out = run_script(b"a AUTHENTICATE XOAUTH2 dXNlcj1hbGljZQ==\r\n").await;
        assert!(out.contains("a OK") && out.contains("XOAUTH2"));

        let out = run_script(b"a AUTHENTICATE OAUTHBEARER bjpob3N0PWE=\r\n").await;
        assert!(out.contains("a OK") && out.contains("OAUTHBEARER"));
    }

    #[tokio::test]
    async fn enable_qresync_echoes_back() {
        let out = run_script(b"a LOGIN \"u\" \"p\"\r\nb ENABLE QRESYNC\r\n").await;
        assert!(out.contains("* ENABLED QRESYNC\r\n"), "got: {out:?}");
        assert!(out.contains("b OK ENABLE completed"));
    }

    #[tokio::test]
    async fn enable_unknown_extension_silently_dropped_but_command_succeeds() {
        // Per RFC 5161 the server must not list unknown extensions in
        // the * ENABLED response. The OK still completes.
        let out = run_script(b"a LOGIN \"u\" \"p\"\r\nb ENABLE WAFFLE\r\n").await;
        assert!(!out.contains("WAFFLE"));
        assert!(out.contains("b OK ENABLE completed"));
    }

    #[tokio::test]
    async fn enable_pre_auth_is_bad() {
        let out = run_script(b"a ENABLE QRESYNC\r\n").await;
        assert!(out.contains("a BAD ENABLE requires authentication"));
    }

    // ── LIST / STATUS tests ────────────────────────────────────────

    #[tokio::test]
    async fn list_pre_auth_is_bad() {
        let out = run_script(b"a LIST \"\" \"*\"\r\n").await;
        assert!(out.contains("a BAD LIST requires authentication"));
    }

    #[tokio::test]
    async fn list_emits_every_fixture_mailbox_with_attributes() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb LIST \"\" \"*\"\r\nq LOGOUT\r\n",
            fixture_with_folders(),
        )
        .await;
        // Inbox is forced to "INBOX" and tagged \Inbox.
        assert!(
            out.contains("* LIST (\\Inbox) \"/\" \"INBOX\"\r\n"),
            "got: {out:?}"
        );
        assert!(out.contains("* LIST (\\Archive) \"/\" \"Archive\"\r\n"));
        // Plain folders carry no attributes; nested ones use `/`.
        assert!(out.contains("* LIST () \"/\" \"Projects\"\r\n"));
        assert!(out.contains("* LIST () \"/\" \"Projects/Rust\"\r\n"));
        assert!(out.contains("b OK LIST completed"));
    }

    #[tokio::test]
    async fn list_empty_pattern_returns_delimiter_only() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb LIST \"\" \"\"\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* LIST (\\Noselect) \"/\" \"\"\r\n"));
        assert!(out.contains("b OK LIST completed"));
    }

    #[tokio::test]
    async fn list_exact_pattern_matches_one_folder() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb LIST \"\" \"INBOX\"\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* LIST (\\Inbox) \"/\" \"INBOX\"\r\n"));
        assert!(!out.contains("Archive"));
    }

    #[tokio::test]
    async fn status_returns_messages_unseen_uids() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb STATUS \"INBOX\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)\r\n",
            fixture_with_folders(),
        )
        .await;
        // Inbox has two emails (one $seen, one not). UIDNEXT = exists+1.
        assert!(
            out.contains(
                "* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 1 UIDNEXT 3 UIDVALIDITY 1 HIGHESTMODSEQ 1)"
            ),
            "got: {out:?}"
        );
        assert!(out.contains("b OK STATUS completed"));
    }

    #[tokio::test]
    async fn status_unknown_mailbox_returns_no() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb STATUS \"Ghost\" (MESSAGES)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("b NO STATUS unknown mailbox"));
    }

    #[tokio::test]
    async fn status_pre_auth_is_bad() {
        let out = run_script(b"a STATUS \"INBOX\" (MESSAGES)\r\n").await;
        assert!(out.contains("a BAD STATUS requires authentication"));
    }

    #[test]
    fn parse_two_astrings_handles_quoted_and_atom() {
        assert_eq!(
            parse_two_astrings("\"\" \"*\""),
            Some((String::new(), "*".to_string()))
        );
        assert_eq!(
            parse_two_astrings("ref pat"),
            Some(("ref".to_string(), "pat".to_string()))
        );
        assert_eq!(parse_two_astrings("\"unterminated"), None);
    }

    #[test]
    fn parse_status_args_extracts_name_and_items() {
        let p = parse_status_args("\"INBOX\" (MESSAGES UNSEEN)").unwrap();
        assert_eq!(p.name, "INBOX");
        assert_eq!(p.items, vec!["MESSAGES".to_string(), "UNSEEN".to_string()]);
    }

    #[test]
    fn pattern_matches_handles_star() {
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("INBOX", "INBOX"));
        assert!(pattern_matches("Inbox*", "Inbox/Sub"));
        assert!(pattern_matches("*Sub", "Inbox/Sub"));
        assert!(!pattern_matches("INBOX", "Inbox"));
    }

    // ── SELECT / UID SEARCH tests ──────────────────────────────────

    #[tokio::test]
    async fn select_inbox_emits_required_untagged_responses() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nq LOGOUT\r\n",
            fixture_with_folders(),
        )
        .await;
        // Inbox has 2 emails, one $seen.
        assert!(out.contains("* 2 EXISTS\r\n"), "got: {out:?}");
        assert!(out.contains("* 0 RECENT\r\n"));
        assert!(out.contains("* FLAGS (\\Seen \\Flagged \\Draft \\Answered \\Deleted)\r\n"));
        assert!(out.contains("* OK [PERMANENTFLAGS"));
        assert!(out.contains("* OK [UIDVALIDITY 1]"));
        assert!(out.contains("* OK [UIDNEXT 3]"));
        assert!(out.contains("* OK [HIGHESTMODSEQ 1]"));
        // First unseen is the second email (e2; e1 has $seen).
        assert!(out.contains("* OK [UNSEEN 2]"));
        assert!(out.contains("b OK [READ-WRITE] SELECT completed"));
    }

    #[tokio::test]
    async fn examine_returns_read_only() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb EXAMINE \"INBOX\"\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("b OK [READ-ONLY] EXAMINE completed"));
    }

    #[tokio::test]
    async fn select_unknown_mailbox_returns_no_and_drops_to_authenticated() {
        // After NO SELECT, UID SEARCH (which requires Selected) should
        // also reject.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"Ghost\"\r\nc UID SEARCH ALL\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("b NO SELECT unknown mailbox"));
        assert!(out.contains("c BAD UID requires SELECT"));
    }

    #[tokio::test]
    async fn uid_search_all_returns_uids_in_ascending_order() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID SEARCH ALL\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* SEARCH 1 2\r\n"), "got: {out:?}");
        assert!(out.contains("c OK UID SEARCH completed"));
    }

    #[tokio::test]
    async fn uid_search_range_filters_to_lo_hi() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID SEARCH 2:*\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* SEARCH 2\r\n"), "got: {out:?}");
    }

    #[tokio::test]
    async fn uid_search_empty_match_emits_bare_search_line() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID SEARCH 99:*\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* SEARCH\r\n"));
        assert!(out.contains("c OK UID SEARCH completed"));
    }

    #[tokio::test]
    async fn uid_search_since_uses_received_at() {
        // Fixture timestamps are 2026-01-15. Anything later returns
        // empty.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID SEARCH SINCE 16-Jan-2026\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* SEARCH\r\n"));

        // Anything earlier returns both UIDs.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID SEARCH SINCE 1-Jan-2026\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* SEARCH 1 2"));
    }

    #[tokio::test]
    async fn close_returns_to_authenticated() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc CLOSE\r\nd UID SEARCH ALL\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("c OK CLOSE completed"));
        assert!(out.contains("d BAD UID requires SELECT"));
    }

    #[tokio::test]
    async fn select_pre_auth_is_bad() {
        let out = run_script(b"a SELECT \"INBOX\"\r\n").await;
        assert!(out.contains("a BAD SELECT requires authentication"));
    }

    #[test]
    fn parse_uid_search_recognises_all_range_and_since() {
        assert!(matches!(parse_uid_search("ALL"), Some(SearchCriteria::All)));
        assert!(matches!(
            parse_uid_search("1:*"),
            Some(SearchCriteria::UidRange { lo: 1, hi: None })
        ));
        assert!(matches!(
            parse_uid_search("3:7"),
            Some(SearchCriteria::UidRange { lo: 3, hi: Some(7) })
        ));
        assert!(matches!(
            parse_uid_search("SINCE 1-Jan-2026"),
            Some(SearchCriteria::Since(_))
        ));
        // Bogus criteria fall through.
        assert!(parse_uid_search("FROM \"alice\"").is_none());
    }

    // ── UID FETCH tests ────────────────────────────────────────────

    #[tokio::test]
    async fn uid_fetch_emits_uid_flags_internaldate_and_body() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (UID FLAGS INTERNALDATE BODY.PEEK[])\r\n",
            fixture_with_folders(),
        )
        .await;
        // Each FETCH response starts with "* <seq> FETCH (".
        assert!(out.contains("* 1 FETCH ("), "got: {out:?}");
        assert!(out.contains("* 2 FETCH ("), "got: {out:?}");
        // UID is echoed.
        assert!(out.contains("UID 1"));
        assert!(out.contains("UID 2"));
        // e1 has $seen; e2 doesn't.
        assert!(out.contains("FLAGS (\\Seen)"));
        assert!(out.contains("FLAGS ()"));
        // INTERNALDATE quoted in IMAP date-time format.
        assert!(out.contains("INTERNALDATE \"15-Jan-2026 10:00:00 +0000\""));
        // BODY[] payload is wrapped in a literal {N}\r\n<bytes>.
        assert!(out.contains("BODY[] {"));
        // Headers we synthesize.
        assert!(out.contains("MIME-Version: 1.0"));
        assert!(out.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(out.contains("c OK UID FETCH completed"));
    }

    #[tokio::test]
    async fn uid_fetch_range_filters_to_uid_window() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 2:* (UID)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* 2 FETCH (UID 2)"));
        assert!(!out.contains("* 1 FETCH"));
    }

    #[tokio::test]
    async fn uid_fetch_changedsince_zero_returns_all() {
        // With HIGHESTMODSEQ pinned at 1, CHANGEDSINCE 0 returns
        // everything. CHANGEDSINCE 1 returns nothing.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (FLAGS) (CHANGEDSINCE 0)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* 1 FETCH"));
        assert!(out.contains("* 2 FETCH"));
        assert!(out.contains("c OK UID FETCH completed"));

        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (FLAGS) (CHANGEDSINCE 1)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(!out.contains("* 1 FETCH"));
        assert!(!out.contains("* 2 FETCH"));
        assert!(out.contains("c OK UID FETCH completed"));
    }

    #[tokio::test]
    async fn uid_fetch_pre_select_is_bad() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nc UID FETCH 1:* (UID)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("c BAD UID requires SELECT"));
    }

    #[tokio::test]
    async fn uid_fetch_bad_attr_returns_bad() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (NOSUCHATTR)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("c BAD UID FETCH bad attribute list"));
    }

    #[tokio::test]
    async fn uid_fetch_body_text_returns_just_the_body() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1 (BODY.PEEK[TEXT])\r\n",
            fixture_with_folders(),
        )
        .await;
        // Body for our test fixture is just "x".
        assert!(out.contains("BODY[TEXT] {1}\r\nx)"), "got: {out:?}");
    }

    // ── UID STORE tests ────────────────────────────────────────────

    #[tokio::test]
    async fn uid_store_add_seen_emits_fetch_with_combined_flags() {
        // fixture_with_folders: e1 has $seen, e2 has nothing.
        // After +FLAGS (\Seen \Draft) both end up with \Seen and
        // \Draft.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 1:2 +FLAGS (\\Seen \\Draft)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(
            out.contains("* 1 FETCH (UID 1 FLAGS (\\Draft \\Seen))"),
            "e1 line missing: {out:?}"
        );
        assert!(
            out.contains("* 2 FETCH (UID 2 FLAGS (\\Draft \\Seen))"),
            "e2 line missing: {out:?}"
        );
        assert!(out.contains("c OK UID STORE completed"));
    }

    #[tokio::test]
    async fn uid_store_remove_subtracts_flags() {
        // -FLAGS (\Seen) on e1 (which has only \Seen) leaves an
        // empty flag list.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 1 -FLAGS (\\Seen)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("* 1 FETCH (UID 1 FLAGS ())"), "got: {out:?}");
    }

    #[tokio::test]
    async fn uid_store_replace_overwrites_flag_set() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 1 FLAGS (\\Answered)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(
            out.contains("* 1 FETCH (UID 1 FLAGS (\\Answered))"),
            "got: {out:?}"
        );
        // The replace dropped the \Seen e1 had; the FETCH line should
        // show only \Answered. Not asserting on the broader output
        // because SELECT's untagged FLAGS list legitimately mentions
        // \Seen.
    }

    #[tokio::test]
    async fn uid_store_silent_skips_fetch_lines() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 1:2 +FLAGS.SILENT (\\Seen)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(!out.contains("* 1 FETCH"));
        assert!(!out.contains("* 2 FETCH"));
        assert!(out.contains("c OK UID STORE completed"));
    }

    #[tokio::test]
    async fn uid_store_custom_keyword_passes_through() {
        // Keywords without a backslash prefix are custom, allowed by
        // PERMANENTFLAGS \*. Just round-trips.
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 2 +FLAGS (label_important)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(
            out.contains("* 2 FETCH (UID 2 FLAGS (label_important))"),
            "got: {out:?}"
        );
    }

    #[tokio::test]
    async fn uid_store_pre_select_is_bad() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\nc UID STORE 1 +FLAGS (\\Seen)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("c BAD UID requires SELECT"));
    }

    #[tokio::test]
    async fn uid_store_bad_op_returns_bad() {
        let out = run_script_with(
            b"a LOGIN \"u\" \"p\"\r\n\
              b SELECT \"INBOX\"\r\n\
              c UID STORE 1 NONSENSE (\\Seen)\r\n",
            fixture_with_folders(),
        )
        .await;
        assert!(out.contains("c BAD UID STORE bad flag list"));
    }

    #[test]
    fn parse_store_op_handles_three_kinds_and_silent() {
        let p = parse_store_op("+FLAGS (\\Seen)").unwrap();
        assert_eq!(p.kind, StoreKind::Add);
        assert!(!p.silent);
        assert_eq!(p.flags, vec!["\\Seen".to_string()]);

        let p = parse_store_op("-FLAGS.SILENT (\\Flagged)").unwrap();
        assert_eq!(p.kind, StoreKind::Remove);
        assert!(p.silent);

        let p = parse_store_op("FLAGS \\Answered").unwrap();
        assert_eq!(p.kind, StoreKind::Replace);
        assert_eq!(p.flags, vec!["\\Answered".to_string()]);
    }

    #[test]
    fn parse_uid_set_handles_single_range_and_combo() {
        let s = parse_uid_set("1").unwrap();
        assert!(s.matches(1) && !s.matches(2));
        let s = parse_uid_set("1:3").unwrap();
        assert!(s.matches(1) && s.matches(3) && !s.matches(4));
        let s = parse_uid_set("5:*").unwrap();
        assert!(!s.matches(4) && s.matches(5) && s.matches(999_999));
        let s = parse_uid_set("1,3,5:7").unwrap();
        assert!(s.matches(1) && !s.matches(2) && s.matches(3));
        assert!(s.matches(5) && s.matches(7) && !s.matches(8));
    }

    #[test]
    fn parse_fetch_attrs_recognises_the_v0_set() {
        let a = parse_fetch_attrs("(UID FLAGS INTERNALDATE BODY.PEEK[])").unwrap();
        assert_eq!(
            a,
            vec![
                FetchAttr::Uid,
                FetchAttr::Flags,
                FetchAttr::InternalDate,
                FetchAttr::BodyFull
            ]
        );
        // De-duplicated.
        let a = parse_fetch_attrs("(UID UID FLAGS)").unwrap();
        assert_eq!(a.len(), 2);
        // Bare attribute (no parens) also works.
        let a = parse_fetch_attrs("FLAGS").unwrap();
        assert_eq!(a, vec![FetchAttr::Flags]);
    }

    #[test]
    fn split_attrs_and_modifiers_isolates_changedsince() {
        let (a, m) = split_attrs_and_modifiers("(UID FLAGS) (CHANGEDSINCE 5)");
        assert_eq!(a, "(UID FLAGS)");
        assert_eq!(m, "(CHANGEDSINCE 5)");
        let (a, m) = split_attrs_and_modifiers("(UID)");
        assert_eq!(a, "(UID)");
        assert_eq!(m, "");
    }

    #[test]
    fn parse_select_args_plain_and_condstore() {
        // Bare name, no params.
        let s = parse_select_args("\"INBOX\"").unwrap();
        assert_eq!(s.name, "INBOX");
        assert!(s.qresync.is_none());
        // CONDSTORE select-param accepted, no QRESYNC.
        let s = parse_select_args("\"INBOX\" (CONDSTORE)").unwrap();
        assert_eq!(s.name, "INBOX");
        assert!(s.qresync.is_none());
        // Atom (unquoted) name works too.
        assert_eq!(
            parse_select_args("Archive (CONDSTORE)").unwrap().name,
            "Archive"
        );
    }

    #[test]
    fn parse_select_args_qresync_variants() {
        // uidvalidity + modseq only: no known-UID set.
        let s = parse_select_args("\"INBOX\" (QRESYNC (1 42))").unwrap();
        let qr = s.qresync.unwrap();
        assert!(qr.known_uids.is_none());
        // With a known-UID set the matcher is populated.
        let s = parse_select_args("\"INBOX\" (QRESYNC (1 42 1:3,7))").unwrap();
        let known = s.qresync.unwrap().known_uids.unwrap();
        assert!(known.matches(2));
        assert!(known.matches(7));
        assert!(!known.matches(4));
        // Optional 4th seq-match-data element is skipped, not an error.
        let s = parse_select_args("\"INBOX\" (QRESYNC (1 42 1:3 (1,2,3 1:3)))").unwrap();
        assert!(s.qresync.unwrap().known_uids.is_some());
    }

    #[test]
    fn parse_select_args_rejects_junk() {
        // Unknown select-param.
        assert!(parse_select_args("\"INBOX\" (BOGUS)").is_none());
        // Unbalanced parens.
        assert!(parse_select_args("\"INBOX\" (CONDSTORE").is_none());
        // Trailing junk after the param group.
        assert!(parse_select_args("\"INBOX\" (CONDSTORE) extra").is_none());
        // Non-numeric QRESYNC modseq.
        assert!(parse_select_args("\"INBOX\" (QRESYNC (1 x))").is_none());
    }

    #[test]
    fn format_uid_ranges_collapses_runs() {
        assert_eq!(format_uid_ranges(&[1]), "1");
        assert_eq!(format_uid_ranges(&[1, 2, 3]), "1:3");
        assert_eq!(format_uid_ranges(&[1, 2, 3, 7]), "1:3,7");
        assert_eq!(format_uid_ranges(&[2, 4, 6]), "2,4,6");
        assert_eq!(format_uid_ranges(&[1, 2, 4, 5, 9]), "1:2,4:5,9");
    }

    #[test]
    fn render_rfc822_includes_load_bearing_headers() {
        use crate::fixture::{Address, Body};
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let e = Email {
            id: "e1".into(),
            account_id: "a".into(),
            thread_id: "t1".into(),
            mailbox_ids: vec!["mb".into()],
            keywords: vec![],
            size: 0,
            received_at: ts,
            sent_at: ts,
            from: Some(Address {
                name: Some("Alice".into()),
                email: "alice@example.com".into(),
            }),
            to: vec![Address {
                name: None,
                email: "bob@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            subject: Some("Hello".into()),
            preview: None,
            message_id: vec!["<e1@x>".into()],
            in_reply_to: vec![],
            references: vec![],
            has_attachment: false,
            body: Body::Text("hi\nthere".into()),
            attachments: vec![],
            raw_bytes: None,
        };
        let r = RenderedRfc822::for_email(&e).full;
        assert!(r.contains("From: Alice <alice@example.com>\r\n"));
        assert!(r.contains("To: <bob@example.com>\r\n"));
        assert!(r.contains("Subject: Hello\r\n"));
        assert!(r.contains("Message-ID: <e1@x>\r\n"));
        assert!(r.contains("MIME-Version: 1.0\r\n"));
        assert!(r.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        // Header/body separator.
        assert!(r.contains("8bit\r\n\r\nhi\r\nthere"));
    }

    #[tokio::test]
    async fn shutdown_signal_drops_connection_with_bye() {
        let (server, mut client) = tokio::io::duplex(8192);
        let (tx, rx) = watch::channel(false);
        let fix = fixture();
        let server_task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(
                server,
                fix,
                None,
                crate::oauth::TokenStore::default(),
                crate::request_log::RequestLog::default(),
                crate::latency::LatencyKnob::default(),
                crate::push::PushHub::new(),
                &mut rx,
            )
            .await
        });

        // Read greeting first so the server is parked on the read.
        let mut greeting = vec![0u8; GREETING.len()];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, GREETING.as_bytes());

        tx.send(true).unwrap();
        let mut tail = Vec::new();
        client.read_to_end(&mut tail).await.unwrap();
        let s = String::from_utf8(tail).unwrap();
        assert!(s.contains("* BYE saehrimnir shutting down"));
        server_task.await.unwrap().unwrap();
    }
}
