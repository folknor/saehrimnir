use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use saehrimnir::lua::MockExit;
use saehrimnir::request_log::RequestLog;
use saehrimnir::sentinel::ProtocolPort;
use saehrimnir::{cli, gmail, graph, imap, routes, scenario, sentinel, shutdown, smtp, tls};
use tokio::sync::watch;

/// Hard budget on graceful drain after SIGTERM. Plan-2 acceptance #6
/// requires a clean shutdown within ~1 second.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = cli::Args::parse();

    let scenario = scenario::load(&args.fixture).map_err(|e| format!("fixture: {e}"))?;
    eprintln!(
        "saehrimnir: fixture {:?} loaded ({} mailboxes, {} emails, dispatcher: {})",
        scenario.fixture.name,
        scenario.fixture.mailboxes.len(),
        scenario.fixture.emails.len(),
        if scenario.dispatcher.is_some() { "yes" } else { "no" },
    );
    let fixture = Arc::clone(&scenario.fixture);
    let dispatcher = scenario.dispatcher.clone();

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

    // SMTP listener.
    let smtp_listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", args.smtp_port)).await?;
    let smtp_addr = smtp_listener.local_addr()?;
    eprintln!("saehrimnir: smtp listening on {smtp_addr}");

    // Microsoft Graph listener.
    let graph_listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", args.graph_port)).await?;
    let graph_addr = graph_listener.local_addr()?;
    eprintln!("saehrimnir: graph listening on {graph_addr}");

    // Gmail listener.
    let gmail_listener =
        tokio::net::TcpListener::bind(format!("127.0.0.1:{}", args.gmail_port)).await?;
    let gmail_addr = gmail_listener.local_addr()?;
    eprintln!("saehrimnir: gmail listening on {gmail_addr}");

    sentinel::write_ready(
        &args.readiness_file,
        &[
            ProtocolPort {
                name: "JMAP",
                port: jmap_addr.port(),
            },
            ProtocolPort {
                name: "IMAP",
                port: imap_addr.port(),
            },
            ProtocolPort {
                name: "SMTP",
                port: smtp_addr.port(),
            },
            ProtocolPort {
                name: "GRAPH",
                port: graph_addr.port(),
            },
            ProtocolPort {
                name: "GMAIL",
                port: gmail_addr.port(),
            },
        ],
    )
    .await?;
    eprintln!(
        "saehrimnir: readiness sentinel written: {}",
        args.readiness_file.display()
    );

    // SMTP submission log. Shared between the SMTP listener (writer)
    // and the JMAP HTTP router (reader, via the test-only
    // `/test/smtp/submissions` route). Process-scoped: cleared on
    // restart, otherwise grows for the life of the binary.
    let smtp_log = smtp::SubmissionLog::new();

    // Cross-protocol request log. Cloned into every protocol layer
    // so a single `GET /test/requests` snapshot covers everything.
    let request_log = RequestLog::new();

    let app = routes::router(routes::AppState {
        fixture: Arc::clone(&fixture),
        dispatcher: dispatcher.clone(),
        submission_log: smtp_log.clone(),
        request_log: request_log.clone(),
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

    // IMAP server. Threads the optional Lua dispatcher so reactive
    // callbacks (currently UID FETCH) can fire.
    let imap_shutdown_rx = shutdown_rx.clone();
    let imap_fixture = Arc::clone(&fixture);
    let imap_dispatcher = dispatcher.clone();
    let imap_request_log = Some(request_log.clone());
    let imap_task = tokio::spawn(async move {
        imap::serve(imap_listener, imap_fixture, imap_dispatcher, imap_request_log, imap_shutdown_rx).await
    });

    // SMTP server. The submission log is the same handle exposed to
    // the JMAP HTTP router above, so harness scripts can read
    // captured submissions via `GET /test/smtp/submissions`.
    let smtp_shutdown_rx = shutdown_rx.clone();
    let smtp_log_clone = smtp_log.clone();
    let smtp_dispatcher = dispatcher.clone();
    let smtp_tls = match tls::make_acceptor() {
        Ok(a) => Some(Arc::new(a)),
        Err(e) => {
            eprintln!("saehrimnir: STARTTLS disabled - cert generation failed: {e}");
            None
        }
    };
    let smtp_request_log = Some(request_log.clone());
    let smtp_task = tokio::spawn(async move {
        smtp::serve(
            smtp_listener,
            smtp_log_clone,
            smtp_dispatcher,
            smtp_tls,
            smtp_request_log,
            smtp_shutdown_rx,
        )
        .await
    });
    // Microsoft Graph server.
    let graph_app = graph::router(graph::AppState {
        fixture: Arc::clone(&fixture),
        dispatcher: dispatcher.clone(),
        request_log: request_log.clone(),
    });
    let graph_shutdown_rx = shutdown_rx.clone();
    let graph_task = tokio::spawn(
        axum::serve(graph_listener, graph_app)
            .with_graceful_shutdown(async move {
                let mut rx = graph_shutdown_rx;
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        return;
                    }
                }
            })
            .into_future(),
    );

    // Gmail server.
    let gmail_app = gmail::router(gmail::AppState {
        fixture: Arc::clone(&fixture),
        dispatcher: dispatcher.clone(),
        request_log: request_log.clone(),
    });
    let gmail_shutdown_rx = shutdown_rx.clone();
    let gmail_task = tokio::spawn(
        axum::serve(gmail_listener, gmail_app)
            .with_graceful_shutdown(async move {
                let mut rx = gmail_shutdown_rx;
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        return;
                    }
                }
            })
            .into_future(),
    );

    // Race the OS signal handler against any mock_done()/mock_fail()
    // signal raised by a Lua scenario. If the dispatcher is absent
    // (TOML fixture or no mock_* call) the mock-exit branch resolves
    // never, so we fall through to the signal path normally.
    let mock_exit_dispatcher = dispatcher.clone();
    let mock_exit_fut = async move {
        match mock_exit_dispatcher {
            Some(d) => d.wait_for_exit().await,
            None => std::future::pending::<MockExit>().await,
        }
    };
    tokio::pin!(mock_exit_fut);

    let mock_exit: Option<MockExit> = tokio::select! {
        _ = shutdown::wait_for_signal() => {
            eprintln!("saehrimnir: shutdown signal received, draining (budget {SHUTDOWN_BUDGET:?})");
            None
        }
        exit = &mut mock_exit_fut => {
            match &exit {
                MockExit::Done => {
                    eprintln!("saehrimnir: mock_done() raised, draining (budget {SHUTDOWN_BUDGET:?})");
                }
                MockExit::Fail(reason) => {
                    eprintln!("saehrimnir: mock_fail({reason:?}) raised, draining (budget {SHUTDOWN_BUDGET:?})");
                }
            }
            Some(exit)
        }
    };
    let _ = shutdown_tx.send(true);

    let drain = async move {
        let _ = jmap_task.await;
        let _ = imap_task.await;
        let _ = smtp_task.await;
        let _ = graph_task.await;
        let _ = gmail_task.await;
    };
    match tokio::time::timeout(SHUTDOWN_BUDGET, drain).await {
        Ok(()) => eprintln!("saehrimnir: clean shutdown"),
        Err(_) => eprintln!("saehrimnir: shutdown budget exceeded, exiting"),
    }
    match mock_exit {
        Some(MockExit::Fail(reason)) => Err(format!("scenario mock_fail: {reason}").into()),
        _ => Ok(()),
    }
}
