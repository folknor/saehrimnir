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

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

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

/// Shared, cheap-to-clone handle. Backed by a `Mutex<Vec<_>>`; the
/// log is process-scoped and grows for the life of the binary
/// unless a test clears it via `DELETE /test/requests` or
/// `POST /test/fixture/reset`.
#[derive(Debug, Clone, Default)]
pub struct RequestLog(Arc<Mutex<Vec<RequestEntry>>>);

impl RequestLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry. Lock is held only for the push; the
    /// per-protocol callers build the `RequestEntry` first and
    /// hand it over.
    pub fn push(&self, entry: RequestEntry) {
        self.0
            .lock()
            .expect("request log mutex poisoned")
            .push(entry);
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

    pub fn snapshot(&self) -> Vec<RequestEntry> {
        self.0.lock().expect("request log mutex poisoned").clone()
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
