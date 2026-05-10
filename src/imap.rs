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

use crate::fixture::{Address, Body, Email, Fixture, Mailbox, Role};

/// Greeting line emitted as soon as the connection is accepted, before
/// the client says anything. Per RFC 3501 sec 7.1, an `* OK` greeting
/// puts the connection in the Not Authenticated state.
pub const GREETING: &str = "* OK saehrimnir IMAP4rev1 ready\r\n";

/// Capabilities advertised in response to `CAPABILITY` and on every
/// `OK [CAPABILITY ...]` resp-text. Authenticated set; the
/// pre-auth set adds `LOGINDISABLED`-equivalents only if we ever grow
/// real auth, which v0 does not.
pub const CAPABILITIES: &str = "IMAP4REV1 CONDSTORE QRESYNC";

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
pub async fn serve(
    listener: TcpListener,
    fixture: crate::shared::FixtureHandle,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
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
                        let fix = Arc::clone(&fixture);
                        let disp = dispatcher.clone();
                        let log = request_log.clone();
                        let lat = latency.clone();
                        let mut sd = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, fix, disp, log, lat, &mut sd).await {
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
pub async fn serve_connection<S>(
    stream: S,
    fixture: crate::shared::FixtureHandle,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut conn = Conn {
        reader: BufReader::new(reader),
        writer,
        state: State::NotAuthenticated,
        fixture,
        dispatcher,
        request_log,
        latency,
        selected: None,
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
        conn.dispatch(&line).await?;
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

    async fn dispatch(&mut self, line: &str) -> std::io::Result<()> {
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
        self.request_log.record("imap", recorded, detail);
        match cmd_upper.as_str() {
            "CAPABILITY" => self.cmd_capability(parsed.tag).await,
            "NOOP" => self.cmd_noop(parsed.tag).await,
            "LOGOUT" => self.cmd_logout(parsed.tag).await,
            "LOGIN" => self.cmd_login(parsed.tag, parsed.args).await,
            "AUTHENTICATE" => self.cmd_authenticate(parsed.tag, parsed.args).await,
            "ENABLE" => self.cmd_enable(parsed.tag, parsed.args).await,
            "LIST" => self.cmd_list(parsed.tag, parsed.args).await,
            "STATUS" => self.cmd_status(parsed.tag, parsed.args).await,
            "SELECT" => self.cmd_select(parsed.tag, parsed.args, true).await,
            "EXAMINE" => self.cmd_select(parsed.tag, parsed.args, false).await,
            "UID" => self.cmd_uid(parsed.tag, parsed.args).await,
            "CLOSE" => self.cmd_close(parsed.tag).await,
            other => {
                self.write_line(&format!(
                    "{} BAD {other} not implemented in v0",
                    parsed.tag
                ))
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

    async fn cmd_login(&mut self, tag: &str, _args: &str) -> std::io::Result<()> {
        if self.state != State::NotAuthenticated {
            return self
                .write_line(&format!("{tag} BAD LOGIN only valid pre-auth"))
                .await;
        }
        // v0 accepts any credential. Don't even bother parsing the
        // user/pass quoted strings.
        self.state = State::Authenticated;
        self.write_line(&format!(
            "{tag} OK [CAPABILITY {CAPABILITIES}] LOGIN completed"
        ))
        .await
    }

    async fn cmd_authenticate(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        if self.state != State::NotAuthenticated {
            return self
                .write_line(&format!("{tag} BAD AUTHENTICATE only valid pre-auth"))
                .await;
        }
        let mut parts = args.splitn(2, ' ');
        let mech = parts.next().unwrap_or("").to_ascii_uppercase();
        let initial_response = parts.next().map(str::trim);
        match mech.as_str() {
            "PLAIN" | "XOAUTH2" | "OAUTHBEARER" | "LOGIN" => {
                // SASL-IR: if the client sent the response on the same
                // line, no continuation needed. Otherwise prompt with
                // `+\r\n` and read one continuation line that we
                // discard.
                if initial_response.is_none() {
                    self.write_line("+").await?;
                    let cont = self.read_line().await?;
                    if cont.is_none() {
                        // peer closed mid-handshake
                        return Ok(());
                    }
                    // Per RFC 3501 sec 6.2.2, the client may abort by
                    // sending `*`; if so, return BAD.
                    if cont.as_deref() == Some("*") {
                        return self
                            .write_line(&format!("{tag} BAD AUTHENTICATE aborted"))
                            .await;
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
                self.write_line(&format!(
                    "{tag} NO unsupported SASL mechanism {other:?}"
                ))
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
            return self
                .write_line(&format!("{tag} OK LIST completed"))
                .await;
        }
        // We accept both `*` and exact-name patterns; everything else
        // falls back to substring matching, which is enough for
        // ratatoskr (it only ever sends `*`).
        let _ = reference; // hierarchy reference is unused in v0.
        let entries = list_mailboxes(&self.fix_read());
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
        let entries = list_mailboxes(&self.fix_read());
        let entry = entries.iter().find(|e| e.path.eq_ignore_ascii_case(&parsed.name));
        let Some(entry) = entry else {
            return self
                .write_line(&format!("{tag} NO STATUS unknown mailbox"))
                .await;
        };
        let counts = mailbox_counts(&self.fix_read(), &entry.fixture_id);
        let mut items = Vec::with_capacity(parsed.items.len());
        for item in &parsed.items {
            let pair = match item.to_ascii_uppercase().as_str() {
                "MESSAGES" => format!("MESSAGES {}", counts.exists),
                "UNSEEN" => format!("UNSEEN {}", counts.unseen),
                "RECENT" => "RECENT 0".to_string(),
                "UIDNEXT" => format!("UIDNEXT {}", counts.uidnext),
                "UIDVALIDITY" => "UIDVALIDITY 1".to_string(),
                "HIGHESTMODSEQ" => "HIGHESTMODSEQ 1".to_string(),
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

    async fn cmd_select(
        &mut self,
        tag: &str,
        args: &str,
        read_write: bool,
    ) -> std::io::Result<()> {
        if !self.is_authenticated() {
            return self
                .write_line(&format!("{tag} BAD SELECT requires authentication"))
                .await;
        }
        let name = match parse_one_astring(args) {
            Some(n) => n,
            None => {
                return self
                    .write_line(&format!("{tag} BAD SELECT expects \"name\""))
                    .await;
            }
        };
        let entries = list_mailboxes(&self.fix_read());
        let entry = entries.iter().find(|e| e.path.eq_ignore_ascii_case(&name));
        let Some(entry) = entry else {
            // Per RFC, on NO SELECT the connection drops back to
            // Authenticated state.
            self.state = State::Authenticated;
            self.selected = None;
            return self
                .write_line(&format!("{tag} NO SELECT unknown mailbox"))
                .await;
        };

        // Counts and the first-unseen index are both pure projections
        // into owned data; each helper borrows the fixture only for
        // its own call and the result owns no `&Fixture` references,
        // so the guard drops between calls.
        let counts = mailbox_counts(&self.fix_read(), &entry.fixture_id);
        let first_unseen_idx = {
            let fix = self.fix_read();
            mailbox_messages(&fix, &entry.fixture_id)
                .iter()
                .position(|(_, e)| !e.keywords.iter().any(|k| k == "$seen"))
        };
        let entry_id = entry.fixture_id.clone();

        // Untagged responses, RFC 3501 sec 6.3.1. Order does not
        // matter, but we emit a stable order for byte-determinism.
        self.write_line(&format!("* {} EXISTS", counts.exists)).await?;
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
        self.write_line(&format!("* OK [UIDNEXT {}] predicted next UID", counts.uidnext))
            .await?;
        self.write_line("* OK [HIGHESTMODSEQ 1] modseq pinned").await?;

        let access = if read_write { "READ-WRITE" } else { "READ-ONLY" };
        let verb = if read_write { "SELECT" } else { "EXAMINE" };
        self.state = State::Selected;
        self.selected = Some(entry_id);
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
        self.state = State::Authenticated;
        self.write_line(&format!("{tag} OK CLOSE completed")).await
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
            "EXPUNGE" => self.cmd_uid_expunge(tag, rest).await,
            other => {
                self.write_line(&format!(
                    "{tag} BAD UID {other} not implemented in v0"
                ))
                .await
            }
        }
    }

    async fn cmd_uid_fetch(&mut self, tag: &str, args: &str) -> std::io::Result<()> {
        // Args: "<uid-set> (<attr>...)" or "<uid-set> <attr>" or
        // "<uid-set> (<attrs>) (CHANGEDSINCE <modseq>)" - the
        // CHANGEDSINCE modifier lands in step 6 but we accept the
        // syntax now and ignore the modseq because HIGHESTMODSEQ is
        // pinned at 1 (so CHANGEDSINCE 0 returns everything,
        // CHANGEDSINCE 1+ returns nothing).
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
        let attrs = match parse_fetch_attrs(attrs_str) {
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
                return self
                    .write_line(&format!("{tag} {status} {message}"))
                    .await;
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
        let snapshot: Vec<(u32, u32, Email)> = if changedsince_matches(changedsince) {
            let fix = self.fix_read();
            mailbox_messages(&fix, &selected_id)
                .into_iter()
                .enumerate()
                .filter(|(_, (uid, _))| set.matches(*uid))
                .map(|(live_idx, (uid, email))| {
                    let seq = u32::try_from(live_idx + 1).expect("seq fits in u32");
                    (seq, uid, email.clone())
                })
                .collect()
        } else {
            Vec::new()
        };

        for (seq, uid, email) in snapshot {
            let line = fetch_response_line(seq, uid, &email, &attrs);
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
        let (uid_set_str, after) = match split_after_set(args) {
            Some(p) => p,
            None => {
                return self
                    .write_line(&format!("{tag} BAD UID STORE expects <set> <flag-op> <flags>"))
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
                        emitted.push(format!(
                            "* {uid} FETCH (UID {uid} FLAGS ({flags}))\r\n"
                        ));
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
            let target_id = list_mailboxes(&fix)
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
                                    let uid = u32::try_from(i + 1)
                                        .expect("uid fits in u32");
                                    (uid, id.clone())
                                })
                            })
                            .collect();
                        for (uid, email_id) in source_uids {
                            if !set.matches(uid) {
                                continue;
                            }
                            let Some(idx) =
                                f.emails.iter().position(|e| e.id == email_id)
                            else {
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
                            let uid = u32::try_from(i + 1)
                                .expect("uid fits in u32");
                            (uid, id.clone())
                        })
                    })
                    .enumerate()
                    .map(|(seq, (uid, id))| {
                        let seq = u32::try_from(seq + 1)
                            .expect("seq fits in u32");
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
                            .is_some_and(|e| {
                                e.keywords.iter().any(|k| k == "$deleted")
                            })
                    })
                    .map(|(seq, _, id)| (*seq, id.clone()))
                    .collect();
                victims.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
                seqs = victims.iter().map(|(seq, _)| *seq).collect();
                for (_seq, id) in &victims {
                    let Some(idx) = f.emails.iter().position(|e| &e.id == id)
                    else {
                        continue;
                    };
                    f.emails[idx].mailbox_ids.retain(|m| m != &selected_id);
                    f.retire_uid(&selected_id, id);
                    if f.emails[idx].mailbox_ids.is_empty() {
                        f.emails.remove(idx);
                        diff.email_destroyed.push(id.clone());
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
        let selected_id = self.selected.clone().expect("Selected state requires selected mailbox");
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
            mailbox_messages(&fix, &selected_id)
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

fn list_mailboxes(fixture: &Fixture) -> Vec<ListEntry> {
    let by_id: HashMap<&str, &Mailbox> =
        fixture.mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();
    fixture
        .mailboxes
        .iter()
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
fn mailbox_messages<'a>(fixture: &'a Fixture, mailbox_id: &str) -> Vec<(u32, &'a Email)> {
    let by_id: HashMap<&str, &Email> = fixture
        .emails
        .iter()
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

fn mailbox_counts(fixture: &Fixture, mailbox_id: &str) -> Counts {
    let by_id: HashMap<&str, &Email> = fixture
        .emails
        .iter()
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
        let requested: Vec<String> = self
            .flags
            .iter()
            .map(|f| imap_flag_to_keyword(f))
            .collect();
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
    Some(StoreOp { kind, flags, silent })
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
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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
    let inner = m.strip_prefix('(').and_then(|s| s.strip_suffix(')')).ok_or(())?;
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

/// With `HIGHESTMODSEQ` pinned at 1, `CHANGEDSINCE 0` returns
/// everything (modseq strictly greater than 0 includes our 1) and
/// `CHANGEDSINCE 1+` returns nothing.
fn changedsince_matches(modseq: Option<u64>) -> bool {
    match modseq {
        None => true,
        Some(n) => n < 1,
    }
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

/// string already terminates with `\r\n` and may contain CRLFs inside
/// an IMAP literal block.
fn fetch_response_line(seq: u32, uid: u32, email: &Email, attrs: &[FetchAttr]) -> String {
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
                out.push_str(&format!(
                    "BODY[TEXT] {{{}}}\r\n{}",
                    r.text.len(),
                    r.text
                ));
            }
            FetchAttr::BodyStructure => {
                out.push_str(&format!("BODYSTRUCTURE {}", render_bodystructure(email)));
            }
            FetchAttr::BodyPart(n) => match render_part_n(email, *n) {
                Some(bytes) => out.push_str(&format!(
                    "BODY[{n}] {{{}}}\r\n{bytes}",
                    bytes.len()
                )),
                None => out.push_str(&format!("BODY[{n}] NIL")),
            },
            FetchAttr::BodyPartMime(n) => match render_part_n_mime(email, *n) {
                Some(bytes) => out.push_str(&format!(
                    "BODY[{n}.MIME] {{{}}}\r\n{bytes}",
                    bytes.len()
                )),
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
    push_header(
        &mut out,
        "Date",
        &email.sent_at.to_rfc2822(),
    );
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
        push_header(
            &mut out,
            "Content-Type",
            "text/plain; charset=utf-8",
        );
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
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        encoded.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        encoded.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        encoded.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
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
    let lines = body.matches("\r\n").count() + usize::from(!body.is_empty() && !body.ends_with("\r\n"));
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
    xs.iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
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
            state: "s1".into(),
            account: Account {
                id: "a".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![],
            emails: vec![],
            oauth: crate::fixture::OAuthConfig::default(),
            calendars: vec![],
            events: vec![],
            change_log: crate::fixture::ChangeLog::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            mailbox_uid_history: HashMap::new(),
        })
    }

    fn fixture_with_folders() -> crate::shared::FixtureHandle {
        use crate::fixture::{Body, Email, Mailbox};
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let mk_email = |id: &str, mailbox: &str, seen: bool| Email {
            id: id.into(),
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
            state: "s1".into(),
            account: Account {
                id: "a".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    name: "Archive".into(),
                    role: Some(Role::Archive),
                    parent_id: None,
                    sort_order: Some(1),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-projects".into(),
                    name: "Projects".into(),
                    role: None,
                    parent_id: None,
                    sort_order: Some(2),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-rust".into(),
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
            calendars: vec![],
            events: vec![],
            change_log: crate::fixture::ChangeLog::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            mailbox_uid_history: HashMap::new(),
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
            serve_connection(server, fix, None, crate::request_log::RequestLog::default(), crate::latency::LatencyKnob::default(), &mut rx).await
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
            out.contains("* CAPABILITY IMAP4REV1 CONDSTORE QRESYNC\r\n"),
            "got: {out:?}"
        );
        assert!(out.contains("a1 OK CAPABILITY completed\r\n"));
        for k in ["STARTTLS", "IDLE", "COMPRESS", "NOTIFY", "APPEND", "NAMESPACE"] {
            assert!(!out.contains(k), "advertised banned capability {k}: {out:?}");
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
            out.contains("a OK [CAPABILITY IMAP4REV1 CONDSTORE QRESYNC] LOGIN completed"),
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
            out.contains("a OK [CAPABILITY IMAP4REV1 CONDSTORE QRESYNC] PLAIN authentication accepted"),
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
        assert!(out.contains("+\r\n"), "expected continuation prompt: {out:?}");
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
        let out = run_script(
            b"a AUTHENTICATE XOAUTH2 dXNlcj1hbGljZQ==\r\n",
        )
        .await;
        assert!(out.contains("a OK") && out.contains("XOAUTH2"));

        let out = run_script(
            b"a AUTHENTICATE OAUTHBEARER bjpob3N0PWE=\r\n",
        )
        .await;
        assert!(out.contains("a OK") && out.contains("OAUTHBEARER"));
    }

    #[tokio::test]
    async fn enable_qresync_echoes_back() {
        let out = run_script(
            b"a LOGIN \"u\" \"p\"\r\nb ENABLE QRESYNC\r\n",
        )
        .await;
        assert!(out.contains("* ENABLED QRESYNC\r\n"), "got: {out:?}");
        assert!(out.contains("b OK ENABLE completed"));
    }

    #[tokio::test]
    async fn enable_unknown_extension_silently_dropped_but_command_succeeds() {
        // Per RFC 5161 the server must not list unknown extensions in
        // the * ENABLED response. The OK still completes.
        let out = run_script(
            b"a LOGIN \"u\" \"p\"\r\nb ENABLE WAFFLE\r\n",
        )
        .await;
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
            out.contains("* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 1 UIDNEXT 3 UIDVALIDITY 1 HIGHESTMODSEQ 1)"),
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
        assert!(matches!(
            parse_uid_search("ALL"),
            Some(SearchCriteria::All)
        ));
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
        assert!(
            out.contains("* 1 FETCH (UID 1 FLAGS ())"),
            "got: {out:?}"
        );
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
    fn render_rfc822_includes_load_bearing_headers() {
        use crate::fixture::{Address, Body};
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let e = Email {
            id: "e1".into(),
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
            serve_connection(server, fix, None, crate::request_log::RequestLog::default(), crate::latency::LatencyKnob::default(), &mut rx).await
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
