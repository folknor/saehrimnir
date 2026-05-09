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
use std::mem;
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

    /// Steal the current contents for read-out. The mutex is
    /// released before the (potentially large) materialisation
    /// into `Vec`, so a `GET /test/requests` against a long-lived
    /// process doesn't stall every listener for the duration of
    /// the copy.
    pub fn snapshot(&self) -> Vec<RequestEntry> {
        let mut taken = {
            let mut g = self.0.lock().expect("request log mutex poisoned");
            mem::take(&mut *g)
        };
        let out: Vec<RequestEntry> = taken.iter().cloned().collect();
        let mut g = self.0.lock().expect("request log mutex poisoned");
        if !g.is_empty() {
            // Concurrent pushes landed while we were cloning;
            // splice them after the taken history (preserving
            // chronological order) and re-apply the ring cap.
            taken.append(&mut g);
            while taken.len() > REQUEST_LOG_CAP {
                taken.pop_front();
            }
        }
        *g = taken;
        out
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
