//! IMAP server.
//!
//! Per-connection state machine driven by `serve_connection` over any
//! `AsyncRead + AsyncWrite` (a `TcpStream` in production, a
//! `tokio::io::DuplexStream` in tests). The accept loop in [`serve`]
//! spawns one task per accepted socket and stops accepting when the
//! shared shutdown future fires.
//!
//! v0 scope (see `notes/imap-plan.md`): plaintext only, accept any
//! credential. Currently implemented: greeting, `CAPABILITY`, `NOOP`,
//! `LOGOUT`, `LOGIN`, `AUTHENTICATE` (PLAIN / XOAUTH2 / OAUTHBEARER),
//! `ENABLE QRESYNC`, `LIST`, `STATUS`. Everything else returns
//! tagged `BAD`.

use std::sync::Arc;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::fixture::{Fixture, Mailbox, Role};

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
    #[allow(dead_code)] // wired up in step 4 (SELECT)
    Selected,
    Logout,
}

/// Run the accept loop until `shutdown` flips. Each accepted connection
/// runs in its own task; in-flight connections drop when their socket
/// is closed or when the shutdown signal interrupts a read.
pub async fn serve(
    listener: TcpListener,
    fixture: Arc<Fixture>,
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
                        let mut sd = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, fix, &mut sd).await {
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

/// Drive a single IMAP connection.
pub async fn serve_connection<S>(
    stream: S,
    fixture: Arc<Fixture>,
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
    fixture: Arc<Fixture>,
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
        match cmd_upper.as_str() {
            "CAPABILITY" => self.cmd_capability(parsed.tag).await,
            "NOOP" => self.cmd_noop(parsed.tag).await,
            "LOGOUT" => self.cmd_logout(parsed.tag).await,
            "LOGIN" => self.cmd_login(parsed.tag, parsed.args).await,
            "AUTHENTICATE" => self.cmd_authenticate(parsed.tag, parsed.args).await,
            "ENABLE" => self.cmd_enable(parsed.tag, parsed.args).await,
            "LIST" => self.cmd_list(parsed.tag, parsed.args).await,
            "STATUS" => self.cmd_status(parsed.tag, parsed.args).await,
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
        let entries = list_mailboxes(&self.fixture);
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
        let entries = list_mailboxes(&self.fixture);
        let entry = entries.iter().find(|e| e.path.eq_ignore_ascii_case(&parsed.name));
        let Some(entry) = entry else {
            return self
                .write_line(&format!("{tag} NO STATUS unknown mailbox"))
                .await;
        };
        let counts = mailbox_counts(&self.fixture, &entry.fixture_id);
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
    let by_id: std::collections::HashMap<&str, &Mailbox> =
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

fn mailbox_path(m: &Mailbox, by_id: &std::collections::HashMap<&str, &Mailbox>) -> String {
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

fn mailbox_counts(fixture: &Fixture, mailbox_id: &str) -> Counts {
    let in_box: Vec<_> = fixture
        .emails
        .iter()
        .filter(|e| e.mailbox_ids.iter().any(|id| id == mailbox_id))
        .collect();
    let exists = in_box.len() as u64;
    let unseen = in_box
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .count() as u64;
    Counts {
        exists,
        unseen,
        uidnext: exists + 1,
    }
}

// ── Tiny IMAP-arg parsers ───────────────────────────────────────────

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

    fn fixture() -> Arc<Fixture> {
        Arc::new(Fixture {
            name: "t".into(),
            state: "s1".into(),
            account: Account {
                id: "a".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![],
            emails: vec![],
        })
    }

    fn fixture_with_folders() -> Arc<Fixture> {
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
        };
        Arc::new(Fixture {
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
        })
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

    async fn run_script_with(script: &[u8], fix: Arc<Fixture>) -> String {
        let (server, mut client) = tokio::io::duplex(8192);
        let (_tx, rx) = watch::channel(false);
        let server_task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(server, fix, &mut rx).await
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
            Some(("".to_string(), "*".to_string()))
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

    #[tokio::test]
    async fn shutdown_signal_drops_connection_with_bye() {
        let (server, mut client) = tokio::io::duplex(8192);
        let (tx, rx) = watch::channel(false);
        let fix = fixture();
        let server_task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(server, fix, &mut rx).await
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
