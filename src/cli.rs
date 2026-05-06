use std::path::PathBuf;

use clap::Parser;

/// Deterministic mock email-protocol server.
///
/// Loads a fixture, binds one TCP port per protocol, writes a readiness
/// sentinel, and serves until SIGTERM. Designed to be spawned by brokkr's
/// `[ratatoskr]` sync commands.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// JMAP HTTP port. `0` (default) picks an ephemeral port; the
    /// chosen port lands in the readiness file under `READY <port>`.
    #[arg(long = "jmap-port", alias = "port", default_value_t = 0)]
    pub jmap_port: u16,

    /// IMAP TCP port. `0` (default) picks an ephemeral port; the
    /// chosen port lands in the readiness file under `IMAP <port>`.
    #[arg(long = "imap-port", default_value_t = 0)]
    pub imap_port: u16,

    /// SMTP submission port. `0` (default) picks an ephemeral port;
    /// the chosen port lands in the readiness file under `SMTP <port>`.
    #[arg(long = "smtp-port", default_value_t = 0)]
    pub smtp_port: u16,

    /// Microsoft Graph mock port. `0` (default) picks an ephemeral
    /// port; the chosen port lands in the readiness file under
    /// `GRAPH <port>`. Mounts `/v1.0/me/...` mail-sync endpoints.
    #[arg(long = "graph-port", default_value_t = 0)]
    pub graph_port: u16,

    /// Gmail mock port. `0` (default) picks an ephemeral port; the
    /// chosen port lands in the readiness file under `GMAIL <port>`.
    /// Mounts `/gmail/v1/users/me/...` endpoints.
    #[arg(long = "gmail-port", default_value_t = 0)]
    pub gmail_port: u16,

    /// Path to write the readiness sentinel once both listeners are
    /// bound. One line per protocol, e.g.:
    /// `READY 12345\nIMAP 23456\n`.
    #[arg(long)]
    pub readiness_file: PathBuf,

    /// Path to the TOML fixture file.
    #[arg(long)]
    pub fixture: PathBuf,

    /// Optional log file path. Logs go to stderr if absent.
    #[arg(long)]
    pub log_file: Option<PathBuf>,
}
