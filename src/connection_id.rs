//! Per-accepted-TCP-connection ids for the cross-protocol request log.
//!
//! Process-wide monotonic `u64` allocated once per accepted socket
//! (IMAP / SMTP `serve_connection`, and per axum HTTP connection via
//! the `Connected` impl below). Stamped into every `RequestEntry`
//! recorded on that connection so harness scripts can group entries
//! by session without sæhrimnir locking in an aggregation schema.
//!
//! Counter starts at 1; 0 is never issued. Resets on process
//! restart only - same lifecycle as `RequestEntry::received_at`,
//! which is also wall-clock and not byte-stable across runs.
//! `?stable=true` on `/test/requests` normalizes raw ids to dense
//! first-seen indices (see `routes::requests_get`).

use std::sync::atomic::{AtomicU64, Ordering};

static GEN: AtomicU64 = AtomicU64::new(1);

/// Allocate the next connection id. Cheap (one atomic add); safe to
/// call from any thread / task.
pub fn next() -> u64 {
    GEN.fetch_add(1, Ordering::Relaxed)
}

/// Connection metadata extracted by axum's `Connected` plumbing.
/// Carries both the peer's `SocketAddr` (for parity with the
/// default `SocketAddr`-based `Connected` impl) and the freshly
/// minted `connection_id`. Handlers pull this via an
/// `Option<ConnectInfo<ConnInfo>>` extractor; the `Option`
/// covers `tower::ServiceExt::oneshot` test paths that bypass
/// `into_make_service_with_connect_info`.
#[derive(Clone, Debug)]
pub struct ConnInfo {
    pub peer: std::net::SocketAddr,
    pub id: u64,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for ConnInfo
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        ConnInfo {
            peer: *stream.remote_addr(),
            id: next(),
        }
    }
}

/// Optional-connection-id extractor that handlers consume to stamp
/// `RequestEntry::connection_id`. Wraps the `Option<u64>` part of
/// `ConnectInfo<ConnInfo>`: present when axum's
/// `into_make_service_with_connect_info::<ConnInfo>` is in front of
/// the router (production / lifecycle test path), absent when the
/// test bypasses it via `tower::ServiceExt::oneshot` or
/// `axum_test`-style direct construction.
///
/// Custom extractor (rather than `Option<ConnectInfo<ConnInfo>>`)
/// because axum 0.8's `ConnectInfo<T>` only implements
/// `FromRequestParts`, not `OptionalFromRequestParts`, so wrapping
/// in `Option` doesn't compile. Going through this newtype keeps
/// the extraction infallible end to end.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptConnId(pub Option<u64>);

impl<S> axum::extract::FromRequestParts<S> for OptConnId
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptConnId(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<ConnInfo>>()
                .map(|c| c.id),
        ))
    }
}
