use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use saehrimnir::sentinel::ProtocolPort;
use saehrimnir::{cli, fixture, imap, routes, sentinel, shutdown};
use tokio::sync::watch;

/// Hard budget on graceful drain after SIGTERM. Plan-2 acceptance #6
/// requires a clean shutdown within ~1 second.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = cli::Args::parse();

    let fixture = fixture::load(&args.fixture).map_err(|e| format!("fixture: {e}"))?;
    eprintln!(
        "saehrimnir: fixture {:?} loaded ({} mailboxes, {} emails)",
        fixture.name,
        fixture.mailboxes.len(),
        fixture.emails.len()
    );
    let fixture = Arc::new(fixture);

    // JMAP listener.
    let jmap_listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", args.jmap_port)).await?;
    let jmap_addr = jmap_listener.local_addr()?;
    eprintln!("saehrimnir: jmap listening on {jmap_addr}");

    // IMAP listener.
    let imap_listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", args.imap_port)).await?;
    let imap_addr = imap_listener.local_addr()?;
    eprintln!("saehrimnir: imap listening on {imap_addr}");

    sentinel::write_ready(
        &args.readiness_file,
        &[
            ProtocolPort {
                name: "READY",
                port: jmap_addr.port(),
            },
            ProtocolPort {
                name: "IMAP",
                port: imap_addr.port(),
            },
        ],
    )
    .await?;
    eprintln!(
        "saehrimnir: readiness sentinel written: {}",
        args.readiness_file.display()
    );

    let app = routes::router(routes::AppState {
        fixture: Arc::clone(&fixture),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // JMAP server via axum + watch-driven graceful shutdown.
    let jmap_shutdown_rx = shutdown_rx.clone();
    let jmap_task = tokio::spawn(
        axum::serve(jmap_listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = jmap_shutdown_rx;
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        return;
                    }
                }
            })
            .into_future(),
    );

    // IMAP server.
    let imap_shutdown_rx = shutdown_rx.clone();
    let imap_fixture = Arc::clone(&fixture);
    let imap_task = tokio::spawn(async move {
        imap::serve(imap_listener, imap_fixture, imap_shutdown_rx).await
    });

    shutdown::wait_for_signal().await;
    eprintln!("saehrimnir: shutdown signal received, draining (budget {SHUTDOWN_BUDGET:?})");
    let _ = shutdown_tx.send(true);

    let drain = async move {
        let _ = jmap_task.await;
        let _ = imap_task.await;
    };
    match tokio::time::timeout(SHUTDOWN_BUDGET, drain).await {
        Ok(()) => {
            eprintln!("saehrimnir: clean shutdown");
            Ok(())
        }
        Err(_) => {
            eprintln!("saehrimnir: shutdown budget exceeded, exiting");
            Ok(())
        }
    }
}
