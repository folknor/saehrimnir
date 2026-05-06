//! Library surface for `saehrimnir`.
//!
//! `main.rs` keeps the runtime entry (signal handling, sentinel,
//! graceful shutdown). Everything else lives here so integration tests
//! can drive the router via `tower::ServiceExt::oneshot` without
//! binding a port.

pub mod cli;
pub mod fixture;
pub mod gmail;
pub mod graph;
pub mod imap;
pub mod jmap;
pub mod lua;
pub mod routes;
pub mod sentinel;
pub mod shutdown;
pub mod smtp;
pub mod templates;
