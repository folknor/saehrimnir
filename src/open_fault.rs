//! Forced JMAP session-open / account-establishment failure knob.
//!
//! Test-only fault injection so a harness can drive a consumer's
//! account-reopen budget to exhaustion. bifrost establishes a JMAP
//! account by fetching the session resource (`GET /jmap/session`,
//! `crate::routes::session`); a mid-sync fatal condition drives the
//! account into a restart that retries that open a bounded number of
//! times (its reopen budget). To exercise the budget-exhaustion path
//! the mock has to fail the session fetch itself for several
//! consecutive attempts - a method-level error (the Lua `on()`
//! dispatcher) never reaches the open path, and token revocation
//! surfaces as a 401 the client treats as an auth problem rather than
//! an open failure.
//!
//! This knob is the connection/auth-establishment analogue of token
//! revocation (`crate::oauth::TokenStore::invalidate`): a shared,
//! cheap-to-clone handle on [`crate::shared::SharedHandles`], armed and
//! cleared over the `/test/*` control plane
//! (`POST` / `DELETE /test/jmap/fail-open`) and reset by
//! `POST /test/fixture/reset`. The `session` handler consumes one
//! attempt from it before doing any work; an armed knob short-circuits
//! the fetch with an HTTP error so the client's open fails.
//!
//! Defaults to disarmed; a fresh process forces no failures.

use std::sync::{Arc, Mutex};

/// Default HTTP status returned on a forced session-open failure.
/// 503 models a transient "server can't establish the session right
/// now" condition - the natural shape for a reopen that should fail
/// and (once cleared) succeed on a later attempt.
pub const DEFAULT_FAIL_STATUS: u16 = 503;

/// Default error detail surfaced in the JMAP-shaped failure body.
pub const DEFAULT_FAIL_MESSAGE: &str = "session establishment forced to fail by test harness";

/// Shared, cheap-to-clone forced-open-failure handle. `None` inside
/// the mutex means disarmed (the default); `Some` carries the live
/// arming.
#[derive(Debug, Clone, Default)]
pub struct OpenFault(Arc<Mutex<Option<Armed>>>);

#[derive(Debug, Clone)]
struct Armed {
    /// Remaining forced failures. `None` = sticky (fail every open
    /// until explicitly cleared); `Some(n)` = fail the next `n`
    /// consecutive opens then auto-disarm.
    remaining: Option<u64>,
    /// HTTP status returned on a forced failure.
    status: u16,
    /// Error detail surfaced in the failure body.
    message: String,
}

impl OpenFault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm the fault. `count = Some(n)` fails the next `n` consecutive
    /// session opens then auto-clears; `count = None` fails every open
    /// until [`OpenFault::clear`]. `count = Some(0)` is treated as a
    /// clear (no failures requested). Replaces any prior arming.
    pub fn arm(&self, count: Option<u64>, status: u16, message: String) {
        let mut g = self.0.lock().expect("open-fault mutex poisoned");
        if count == Some(0) {
            *g = None;
        } else {
            *g = Some(Armed {
                remaining: count,
                status,
                message,
            });
        }
    }

    /// Consume one open attempt. Returns `Some((status, message))` when
    /// this attempt should fail, decrementing a finite budget and
    /// auto-clearing when it reaches zero. Returns `None` when
    /// disarmed (the open proceeds normally).
    pub fn take(&self) -> Option<(u16, String)> {
        let mut g = self.0.lock().expect("open-fault mutex poisoned");
        // Move the arming out (leaving the knob disarmed), mutate the
        // owned copy, then re-arm unless a finite budget just hit zero.
        // Taking it out sidesteps the borrow conflict of reassigning
        // the guarded `Option` while a `&mut Armed` is still live.
        let mut armed = g.take()?;
        let out = (armed.status, armed.message.clone());
        match armed.remaining.as_mut() {
            Some(n) => {
                *n = n.saturating_sub(1);
                if *n > 0 {
                    *g = Some(armed);
                }
                // else: finite budget exhausted - stay disarmed.
            }
            None => {
                // Sticky: re-arm unchanged.
                *g = Some(armed);
            }
        }
        Some(out)
    }

    /// Disarm so the next open succeeds. Idempotent.
    pub fn clear(&self) {
        *self.0.lock().expect("open-fault mutex poisoned") = None;
    }

    /// Observability snapshot of the remaining budget: `None` when
    /// disarmed, `Some(None)` when sticky, `Some(Some(n))` when `n`
    /// forced failures remain. Used by the arm route to echo state
    /// back for round-trip verification.
    pub fn snapshot(&self) -> Option<Option<u64>> {
        self.0
            .lock()
            .expect("open-fault mutex poisoned")
            .as_ref()
            .map(|a| a.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disarmed_by_default() {
        let f = OpenFault::new();
        assert!(f.take().is_none());
        assert_eq!(f.snapshot(), None);
    }

    #[test]
    fn finite_budget_fails_exactly_n_then_succeeds() {
        let f = OpenFault::new();
        f.arm(Some(3), 503, "boom".to_string());
        // Three consecutive opens fail with the armed status/message.
        for _ in 0..3 {
            assert_eq!(f.take(), Some((503, "boom".to_string())));
        }
        // The fourth open (the eventual resume reopen) succeeds.
        assert!(f.take().is_none());
        assert_eq!(f.snapshot(), None);
    }

    #[test]
    fn sticky_fails_until_cleared() {
        let f = OpenFault::new();
        f.arm(None, 503, "down".to_string());
        for _ in 0..5 {
            assert!(f.take().is_some());
        }
        assert_eq!(f.snapshot(), Some(None));
        f.clear();
        assert!(f.take().is_none());
    }

    #[test]
    fn arm_zero_is_a_clear() {
        let f = OpenFault::new();
        f.arm(Some(2), 503, "x".to_string());
        f.arm(Some(0), 503, "x".to_string());
        assert!(f.take().is_none());
    }
}
