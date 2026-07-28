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

use std::sync::{Arc, Mutex, RwLock};

use crate::fixture::Fixture;
use crate::latency::LatencyKnob;
use crate::lua::Dispatcher;
use crate::oauth::TokenStore;
use crate::open_fault::OpenFault;
use crate::push::PushHub;
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
    /// Immutable post-load snapshot of the fixture. Cloned once at
    /// startup and held behind an `Arc` so cloning the handle bag
    /// is still a pointer bump. `POST /test/fixture/reset` rewinds
    /// `fixture` to this image so a harness can re-run the same
    /// `[[change]]` script in one process. Cleared on TLS test
    /// helpers via `for_test`, which seeds it from the live
    /// fixture (the test never mutates).
    pub baseline: Arc<Fixture>,
    /// Cursor over `Fixture::change_script`. Advanced by
    /// `POST /test/fixture/step`; reset to 0 by
    /// `POST /test/fixture/reset`. Process-scoped; never
    /// persisted.
    pub change_cursor: Arc<Mutex<usize>>,
    pub dispatcher: Option<Arc<Dispatcher>>,
    pub request_log: RequestLog,
    pub token_store: TokenStore,
    /// Per-protocol latency knob. Each protocol's dispatch entry
    /// calls `latency.sleep_for("<protocol>").await` once per
    /// command before doing real work; the global knob applies on
    /// top of any per-protocol setting. Defaults to all-zero so
    /// fixtures that don't drive `POST /test/set-latency` see no
    /// delay.
    pub latency: LatencyKnob,
    /// Provider push hub: the shared registry behind the JMAP
    /// WebSocket, Gmail Pub/Sub, and Graph webhook surfaces. Cloning
    /// is an `Arc` bump. The test-admin step path fires it on every
    /// state advance; the client-facing subscribe endpoints register
    /// into it. See `crate::push`.
    pub push: PushHub,
    /// Forced JMAP session-open failure knob. Armed / cleared over the
    /// `/test/jmap/fail-open` control plane; the `session` handler
    /// consumes one attempt from it before serving the session
    /// resource so a harness can drive a consumer's account-reopen
    /// budget to exhaustion. Defaults to disarmed. Cleared by
    /// `POST /test/fixture/reset`. See `crate::open_fault`.
    pub open_fault: OpenFault,
    /// Per-trigger match counts for the fixture's announce triggers.
    /// Process-volatile; cleared by `POST /test/fixture/reset`. See
    /// `crate::announce`.
    pub announce_counts: crate::announce::AnnounceCounters,
}

impl SharedHandles {
    /// Build a SharedHandles around `fixture` with fresh, default
    /// log and token-store handles and no dispatcher attached.
    /// Used by tests that don't need to drive a specific log; the
    /// per-router `AppState::for_test` helpers funnel through
    /// here.
    pub fn for_test(fixture: FixtureHandle) -> Self {
        let baseline = Arc::new(fixture.read().expect("fixture lock poisoned").clone());
        Self {
            fixture,
            baseline,
            change_cursor: Arc::new(Mutex::new(0)),
            dispatcher: None,
            request_log: RequestLog::default(),
            token_store: TokenStore::default(),
            latency: LatencyKnob::default(),
            push: PushHub::new(),
            open_fault: OpenFault::default(),
            announce_counts: crate::announce::AnnounceCounters::default(),
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
