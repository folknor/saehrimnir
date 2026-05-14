//! Per-protocol latency knob.
//!
//! Test-only delay injection so harness scripts can drive sync-bench
//! scenarios under reproducible timing. Each protocol's dispatch
//! entry calls [`LatencyKnob::sleep_for`] once per command (or once
//! per HTTP request) before doing real work; the sleep duration is
//! the sum of the `"global"` setting and the per-protocol setting
//! for that key.
//!
//! Mutated by `POST /test/set-latency` and read by
//! `GET /test/latency`. Reset by `POST /test/fixture/reset`. Lives
//! on `crate::shared::SharedHandles` so every protocol's `AppState`
//! sees the same view.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared, cheap-to-clone latency-knob handle. Backed by a
/// `Mutex<HashMap<String, u64>>` keyed by protocol tag (`"jmap"` /
/// `"imap"` / `"smtp"` / `"graph"` / `"gmail"`) plus the special
/// `"global"` key applied to every protocol. Values are
/// milliseconds.
///
/// Beyond the per-protocol command sleep, the `"attachment"` tag
/// fires once before serving attachment bytes on every protocol
/// that exposes a per-attachment fetch: JMAP `/jmap/download/...`,
/// Gmail `/messages/{id}/attachments/{aid}`, Graph
/// `/messages/{id}/attachments/{aid}` (both the JSON-with-bytes
/// and the `/$value` raw-bytes variants), and IMAP `FETCH BODY[N]`
/// for `N >= 2` (the attachment-part path; `BODY[1]` is the text
/// body and skipped). Lets a harness script race a SIGINT against
/// an in-flight attachment fetch without flake.
///
/// Per-attachment overrides keyed by blob_id stack on top of
/// `"attachment"`: setting `"attachment:<blob_id>"` to N adds N ms
/// to the sleep that fires when that specific attachment is served,
/// so a script can stage one slow attachment among several fast
/// ones (set `"attachment"` to 0 to keep the others instant).
/// Stacks symmetrically across all four protocols since each
/// attachment carries the same `blob_id` regardless of wire shape.
///
/// Defaults to all-zero; a fresh process applies no latency.
#[derive(Debug, Clone, Default)]
pub struct LatencyKnob(Arc<Mutex<HashMap<String, u64>>>);

impl LatencyKnob {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute the effective delay for `protocol` and sleep that
    /// long under tokio. The lock is held only long enough to read
    /// the two relevant entries; the actual sleep is unlocked.
    pub async fn sleep_for(&self, protocol: &str) {
        let ms = self.lookup(protocol);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    /// Synchronous variant for the IMAP / SMTP dispatch loops, which
    /// already block on tokio I/O reads but accept that the
    /// per-command sleep is part of the connection timeline.
    pub fn lookup(&self, protocol: &str) -> u64 {
        let g = self.0.lock().expect("latency knob mutex poisoned");
        let global = g.get("global").copied().unwrap_or(0);
        let specific = g.get(protocol).copied().unwrap_or(0);
        global.saturating_add(specific)
    }

    /// Compute the effective delay for serving a specific attachment
    /// and sleep that long. The sleep is `global + attachment +
    /// attachment:<blob_id>`, so a script can leave the base
    /// `attachment` knob at 0 and stage one slow blob via
    /// `attachment:<blob_id>` without slowing the others.
    pub async fn sleep_for_attachment(&self, blob_id: &str) {
        let ms = {
            let g = self.0.lock().expect("latency knob mutex poisoned");
            let global = g.get("global").copied().unwrap_or(0);
            let base = g.get("attachment").copied().unwrap_or(0);
            let per = g
                .get(&format!("attachment:{blob_id}"))
                .copied()
                .unwrap_or(0);
            global.saturating_add(base).saturating_add(per)
        };
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    /// Set a per-key value. Pass `protocol = "global"` for the
    /// global knob.
    pub fn set(&self, protocol: &str, ms: u64) {
        let mut g = self.0.lock().expect("latency knob mutex poisoned");
        if ms == 0 {
            g.remove(protocol);
        } else {
            g.insert(protocol.to_string(), ms);
        }
    }

    /// Snapshot the current settings as a sorted (BTreeMap-style)
    /// map for byte-stable serialisation.
    pub fn snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        let g = self.0.lock().expect("latency knob mutex poisoned");
        g.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Drop every entry. Called by `POST /test/fixture/reset` so a
    /// harness gets a clean slate without restarting the binary.
    pub fn clear(&self) {
        self.0.lock().expect("latency knob mutex poisoned").clear();
    }
}
