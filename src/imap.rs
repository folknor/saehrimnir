//! IMAP server.
//!
//! Per-connection state machine driven by `serve_connection` over any
//! `AsyncRead + AsyncWrite` (a `TcpStream` in production, a
//! `tokio::io::DuplexStream` in tests). The accept loop in
//! [`serve`] spawns one task per accepted socket and stops accepting
//! when the shared shutdown future fires.
//!
//! v0 scope (see `notes/imap-plan.md`): plaintext only, no auth, no
//! folders, no fetch. This file implements the bootstrap subset:
//! greeting + `CAPABILITY`, `NOOP`, `LOGOUT`. Everything else returns
//! tagged `BAD`.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::fixture::Fixture;

/// Greeting line emitted as soon as the connection is accepted, before
/// the client says anything. Per RFC 3501 sec 7.1, an `* OK` greeting
/// puts the connection in the Not Authenticated state.
pub const GREETING: &str = "* OK saehrimnir IMAP4rev1 ready\r\n";

/// Capabilities advertised in response to `CAPABILITY`. `IMAP4REV1` is
/// the baseline; `CONDSTORE` and `QRESYNC` are required by ratatoskr's
/// resync code path. Everything else (IDLE, NOTIFY, COMPRESS, STARTTLS,
/// APPEND, NAMESPACE, etc.) is deliberately omitted - see
/// `notes/ratatoskr-imap-surface.md`.
pub const CAPABILITIES: &str = "IMAP4REV1 CONDSTORE QRESYNC";

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

/// Drive a single IMAP connection. Generic over the byte stream so
/// tests can wire up a `tokio::io::DuplexStream` without binding a
/// real socket.
pub async fn serve_connection<S>(
    stream: S,
    fixture: Arc<Fixture>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = fixture; // consumed by later steps; threaded through now.

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    writer.write_all(GREETING.as_bytes()).await?;
    writer.flush().await?;

    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Best-effort BYE; ignore errors, the peer may be gone.
                    let _ = writer
                        .write_all(b"* BYE saehrimnir shutting down\r\n")
                        .await;
                    let _ = writer.flush().await;
                    return Ok(());
                }
                continue;
            }
            r = reader.read_line(&mut line) => r?,
        };
        if read == 0 {
            // Peer closed.
            return Ok(());
        }
        let request = strip_crlf(&line);
        let outcome = handle_command(request);
        for resp in outcome.responses {
            writer.write_all(resp.as_bytes()).await?;
            writer.write_all(b"\r\n").await?;
        }
        writer.flush().await?;
        if outcome.close {
            return Ok(());
        }
    }
}

fn strip_crlf(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Result of handling one client command line.
struct Outcome {
    /// Lines to emit in order. Each is written followed by `\r\n`.
    responses: Vec<String>,
    /// If true, the connection closes after these responses are sent.
    close: bool,
}

fn handle_command(request: &str) -> Outcome {
    let parsed = match parse_command_line(request) {
        Some(p) => p,
        None => {
            // Per RFC 3501 sec 9, a malformed line gets `* BAD` (no
            // tag because we couldn't extract one). Don't close - the
            // client may recover.
            return Outcome {
                responses: vec![format!("* BAD malformed command line")],
                close: false,
            };
        }
    };

    let cmd = parsed.command.to_ascii_uppercase();
    match cmd.as_str() {
        "CAPABILITY" => Outcome {
            responses: vec![
                format!("* CAPABILITY {CAPABILITIES}"),
                format!("{} OK CAPABILITY completed", parsed.tag),
            ],
            close: false,
        },
        "NOOP" => Outcome {
            responses: vec![format!("{} OK NOOP completed", parsed.tag)],
            close: false,
        },
        "LOGOUT" => Outcome {
            responses: vec![
                "* BYE saehrimnir signing off".to_string(),
                format!("{} OK LOGOUT completed", parsed.tag),
            ],
            close: true,
        },
        other => Outcome {
            responses: vec![format!(
                "{} BAD {other} not implemented in v0",
                parsed.tag
            )],
            close: false,
        },
    }
}

#[derive(Debug)]
struct Parsed<'a> {
    tag: &'a str,
    command: &'a str,
    #[allow(dead_code)] // Consumed by later steps.
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
        let (server, mut client) = tokio::io::duplex(8192);
        let (_tx, rx) = watch::channel(false);
        let fix = fixture();
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
        // Capabilities we deliberately don't advertise.
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
        // After LOGOUT the server returns; the second command is
        // never read or replied to.
        let out = run_script(b"q LOGOUT\r\nx2 NOOP\r\n").await;
        assert!(out.contains("q OK LOGOUT completed\r\n"));
        assert!(!out.contains("x2 OK"));
    }

    #[tokio::test]
    async fn unknown_command_is_bad_not_close() {
        let out = run_script(b"a APPEND inbox\r\nb NOOP\r\n").await;
        assert!(out.contains("a BAD APPEND not implemented"));
        // Connection survived the unknown command - NOOP still ack'd.
        assert!(out.contains("b OK NOOP completed"));
    }

    #[tokio::test]
    async fn case_insensitive_command_matching() {
        let out = run_script(b"a1 capability\r\n").await;
        assert!(out.contains("a1 OK CAPABILITY completed"));
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

        // Flip shutdown.
        tx.send(true).unwrap();
        let mut tail = Vec::new();
        client.read_to_end(&mut tail).await.unwrap();
        let s = String::from_utf8(tail).unwrap();
        assert!(s.contains("* BYE saehrimnir shutting down"));
        server_task.await.unwrap().unwrap();
    }
}
