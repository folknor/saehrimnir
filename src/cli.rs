use std::path::PathBuf;

use clap::Parser;

/// Deterministic mock JMAP server.
///
/// Loads a fixture, binds a TCP port, writes a readiness sentinel, and
/// serves JMAP until SIGTERM. Designed to be spawned by brokkr's
/// `[ratatoskr]` sync commands.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// TCP port to listen on. `0` (default) picks an ephemeral port; the
    /// chosen port is written to the readiness file.
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Path to write `READY <port>\n` once the listener is bound. Brokkr
    /// watches this via `wait_for_sentinel` to know when to launch the
    /// process that drives the workload.
    #[arg(long)]
    pub readiness_file: PathBuf,

    /// Path to the TOML fixture file.
    #[arg(long)]
    pub fixture: PathBuf,

    /// Optional log file path. Logs go to stderr if absent.
    #[arg(long)]
    pub log_file: Option<PathBuf>,
}
