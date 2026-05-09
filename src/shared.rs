//! Shared protocol-handle bag.
//!
//! `SharedHandles` is the four-handle bundle that every HTTP-based
//! `AppState` needs: the fixture, the optional Lua dispatcher, the
//! cross-protocol request log, and the OAuth token store. Each
//! protocol's `AppState` embeds one of these as `pub shared` and
//! adds protocol-specific extras alongside (e.g. `submission_log`
//! and `base_url` on `routes::AppState`).
//!
//! Why not a single `AppState` for all three routers: axum's
//! `State<T>` typing wants distinct types per-router so a Graph
//! handler can't reach for a JMAP-only field by accident, and the
//! protocol-specific extras genuinely don't belong on the others.
//! Sharing only the four genuinely-shared handles is the minimum
//! that keeps the construction sites readable.

use std::sync::{Arc, RwLock};

use crate::fixture::Fixture;
use crate::lua::Dispatcher;
use crate::oauth::TokenStore;
use crate::request_log::RequestLog;

/// Shared, mutex-protected handle to the live `Fixture`. Cloning is
/// cheap (an `Arc` bump). Holders acquire `.read()` for the read
/// paths and `.write()` for the JMAP `Email/set` / `Mailbox/set`
/// mutators (see `notes/fixture-format.md`). The single fixture-level
/// lock fits the read-heavy / narrow-mutation profile and avoids the
/// locking discipline a per-resource scheme would demand. Guards are
/// held only for the duration of an in-memory walk; never across a
/// `.await` or a Lua dispatcher callback (those would deadlock on the
/// next call).
pub type FixtureHandle = Arc<RwLock<Fixture>>;

/// Construct a `FixtureHandle` around an owned `Fixture`. Centralised
/// so test helpers and `main.rs` don't have to spell `Arc::new(
/// RwLock::new(...))` themselves.
pub fn handle(fixture: Fixture) -> FixtureHandle {
    Arc::new(RwLock::new(fixture))
}

#[derive(Clone)]
pub struct SharedHandles {
    pub fixture: FixtureHandle,
    pub dispatcher: Option<Arc<Dispatcher>>,
    pub request_log: RequestLog,
    pub token_store: TokenStore,
}

impl SharedHandles {
    /// Build a SharedHandles around `fixture` with fresh, default
    /// log and token-store handles and no dispatcher attached.
    /// Used by tests that don't need to drive a specific log; the
    /// per-router `AppState::for_test` helpers funnel through
    /// here.
    pub fn for_test(fixture: FixtureHandle) -> Self {
        Self {
            fixture,
            dispatcher: None,
            request_log: RequestLog::default(),
            token_store: TokenStore::default(),
        }
    }

    /// Builder helper: replace the request log handle and return
    /// `self`. Lets a test do
    /// `AppState::for_test(fix).with_request_log(log.clone())`.
    pub fn with_request_log(mut self, log: RequestLog) -> Self {
        self.request_log = log;
        self
    }

    /// Builder helper: attach a dispatcher.
    pub fn with_dispatcher(mut self, dispatcher: Arc<Dispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// Builder helper: replace the token store handle.
    pub fn with_token_store(mut self, store: TokenStore) -> Self {
        self.token_store = store;
        self
    }
}
