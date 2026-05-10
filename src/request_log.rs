//! Cross-protocol request log.
//!
//! Process-scoped, in-memory ring of `RequestEntry` rows that every
//! protocol layer appends to as it dispatches commands. Exposed to
//! tests over `GET /test/requests` (snapshot) and `DELETE
//! /test/requests` (clear) on the JMAP HTTP listener so harness
//! scripts can assert "client did/did not call X" without each test
//! plumbing its own capture. Distinct from `smtp::SubmissionLog`,
//! which captures *parsed messages* rather than per-command
//! envelopes.
//!
//! Determinism note: `received_at` is a wall-clock timestamp, so
//! rendered JSON is *not* byte-identical across runs. Tests that
//! need byte-stable output should assert on `protocol` / `command`
//! / `detail` and ignore `received_at`. The wall clock is the only
//! non-deterministic field saehrimnir emits anywhere; we accept the
//! impurity here because timestamps are useful for diagnosing
//! ordering issues in CI logs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// Maximum number of entries retained in the cross-protocol log.
/// Past this, oldest entries drop off the front. Sized for a long
/// scale-test run (10k-email fixtures with deltas + sweeps); a
/// pathological client still can't grow the log without bound.
pub const REQUEST_LOG_CAP: usize = 100_000;

/// One dispatch event. `protocol` is the lowercase protocol tag
/// (`"jmap"`, `"imap"`, `"smtp"`, `"graph"`, `"gmail"`). `command`
/// is the protocol-native verb: a JMAP method name, an IMAP command
/// keyword, an SMTP verb, or for HTTP-based protocols the request
/// `METHOD path` (query string stripped to keep entries stable).
/// `detail` is a free-form JSON object protocol layers populate
/// with whatever structured extras they have on hand (call ids,
/// account ids, sequence sets, query params).
#[derive(Debug, Clone, Serialize)]
pub struct RequestEntry {
    pub protocol: &'static str,
    pub command: String,
    pub received_at: DateTime<Utc>,
    pub detail: Value,
}

/// Shared, cheap-to-clone handle. Backed by a
/// `Mutex<VecDeque<_>>`; the log is process-scoped and capped at
/// `REQUEST_LOG_CAP` entries (drop-oldest). Tests clear it via
/// `DELETE /test/requests` or `POST /test/fixture/reset`.
///
/// Every method `expect("request log mutex poisoned")`. Critical
/// sections are tiny (push, take, clear) and panic-free, so
/// poisoning is unreachable today. If a future panic *under* the
/// lock surfaces here, the binary dies; that's intentional - a
/// process-wide restart is preferable to silently degraded
/// recording in a test mock.
#[derive(Debug, Clone, Default)]
pub struct RequestLog(Arc<Mutex<VecDeque<RequestEntry>>>);

impl RequestLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry, evicting the oldest if the ring is full.
    /// Lock is held only for the push; the per-protocol callers
    /// build the `RequestEntry` first and hand it over.
    pub fn push(&self, entry: RequestEntry) {
        let mut g = self.0.lock().expect("request log mutex poisoned");
        if g.len() >= REQUEST_LOG_CAP {
            g.pop_front();
        }
        g.push_back(entry);
    }

    /// Convenience: build an entry from `protocol` + `command` +
    /// `detail` and stamp `received_at` to `Utc::now()`. Most
    /// callers should use this rather than constructing
    /// `RequestEntry` by hand.
    pub fn record(&self, protocol: &'static str, command: impl Into<String>, detail: Value) {
        self.push(RequestEntry {
            protocol,
            command: command.into(),
            received_at: Utc::now(),
            detail,
        });
    }

    /// Append many entries under one lock acquisition. Used by
    /// JMAP's `api()` so a 16-call batch costs one mutex round-
    /// trip rather than 16. The ring cap is applied incrementally
    /// inside the loop so a batch larger than `REQUEST_LOG_CAP`
    /// still leaves a coherent (capped) tail.
    pub fn extend(&self, entries: impl IntoIterator<Item = RequestEntry>) {
        let mut g = self.0.lock().expect("request log mutex poisoned");
        for entry in entries {
            if g.len() >= REQUEST_LOG_CAP {
                g.pop_front();
            }
            g.push_back(entry);
        }
    }

    /// Snapshot the current contents for read-out. Holds the mutex
    /// for the duration of the clone so a concurrent `record` /
    /// `extend` blocks briefly (typical: a 100k-entry clone is on
    /// the order of milliseconds; the deque is bounded at
    /// `REQUEST_LOG_CAP`). Pre-fix this used a steal-clone-restore
    /// dance that released the lock during the clone, but a
    /// concurrent push could then arrive and the cap-reapplication
    /// after splicing could evict entries from the LIVE deque that
    /// had just been handed back in the snapshot - a follow-up
    /// `GET /test/requests` would silently return fewer rows.
    pub fn snapshot(&self) -> Vec<RequestEntry> {
        let g = self.0.lock().expect("request log mutex poisoned");
        g.iter().cloned().collect()
    }

    pub fn clear(&self) {
        self.0.lock().expect("request log mutex poisoned").clear();
    }

    pub fn len(&self) -> usize {
        self.0.lock().expect("request log mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Inline mutation-body cap for request-log entries. Bodies whose
/// serialized form exceeds this serialize to
/// `{"truncated": true, "size": N}` instead of the full Value.
/// Tests that assert on body content stay well under this;
/// fixtures with large bodies (calendar invites with embedded
/// VTIMEZONE, etc.) don't blow up the steady-state heap of the
/// 100k-cap log between `DELETE /test/requests` calls.
pub const REQUEST_LOG_BODY_MAX_BYTES: usize = 4096;

/// Wrap `body` in `{"body": ...}` shape suitable for the
/// `RequestEntry::detail` field, truncating when the serialized
/// form exceeds [`REQUEST_LOG_BODY_MAX_BYTES`].
pub fn body_detail(body: &Value) -> Value {
    let serialized = body.to_string();
    if serialized.len() > REQUEST_LOG_BODY_MAX_BYTES {
        return serde_json::json!({
            "body": {
                "truncated": true,
                "size": serialized.len(),
            }
        });
    }
    serde_json::json!({ "body": body })
}
