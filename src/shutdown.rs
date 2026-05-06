/// Resolve when SIGTERM or SIGINT (Ctrl-C) arrives.
///
/// On non-unix platforms, falls back to Ctrl-C only. The mock is
/// brokkr-spawned on Linux in production; the windows path is just
/// "compiles cleanly for `cargo check` on every host".
#[cfg(unix)]
pub async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}

#[cfg(not(unix))]
pub async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
