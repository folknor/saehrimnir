//! Library surface for `saehrimnir`.
//!
//! `main.rs` keeps the runtime entry (signal handling, sentinel,
//! graceful shutdown). Everything else lives here so integration tests
//! can drive the router via `tower::ServiceExt::oneshot` without
//! binding a port.

// `unwrap_used` is denied at the crate level for production safety, but
// in test code panicking is the assertion mechanism - keep it
// idiomatic.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod cli;
pub mod fixture;
pub mod gmail;
pub mod graph;
pub mod imap;
pub mod jmap;
pub mod latency;
pub mod lua;
pub mod oauth;
pub mod request_log;
pub mod routes;
pub mod scenario;
pub mod sentinel;
pub mod shared;
pub mod shutdown;
pub mod smtp;
pub mod templates;
pub mod tls;
