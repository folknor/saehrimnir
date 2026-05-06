mod cli;
mod fixture;
mod sentinel;
mod shutdown;

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get};
use clap::Parser;

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
    let _fixture: Arc<fixture::Fixture> = Arc::new(fixture);

    let app = Router::new().route("/", get(|| async { "saehrimnir\n" }));

    let bind_addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    let local_addr = listener.local_addr()?;
    eprintln!("saehrimnir: listening on {local_addr}");

    sentinel::write_ready(&args.readiness_file, local_addr.port()).await?;
    eprintln!(
        "saehrimnir: readiness sentinel written: {}",
        args.readiness_file.display()
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .into_future(),
    );

    shutdown::wait_for_signal().await;
    eprintln!("saehrimnir: shutdown signal received, draining (budget {SHUTDOWN_BUDGET:?})");
    let _ = shutdown_tx.send(());

    match tokio::time::timeout(SHUTDOWN_BUDGET, server).await {
        Ok(Ok(Ok(()))) => {
            eprintln!("saehrimnir: clean shutdown");
            Ok(())
        }
        Ok(Ok(Err(e))) => Err(e.into()),
        Ok(Err(e)) => Err(format!("server task panicked: {e}").into()),
        Err(_) => {
            eprintln!("saehrimnir: shutdown budget exceeded, exiting");
            Ok(())
        }
    }
}
