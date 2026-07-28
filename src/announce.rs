//! Request-ordered change-script triggers: the mock's affordance for
//! the cold-start backfill race.
//!
//! A consumer opening an account does two things at once - it walks a
//! paginated inventory (the backfill) and it consumes a live change
//! feed. The interesting behaviour lives at the seam: an object the
//! feed announces WHILE the walk is in flight may also be served by
//! the walk, and the consumer has to reconcile the two.
//!
//! `POST /test/fixture/step` cannot stage that. It only ever
//! interleaves between whole request/response pairs, and the consumer
//! decides when it asks - so a harness can put a change before or
//! after a backfill, but never DURING one. A fixture's
//! [`crate::fixture::AnnounceTrigger`] closes that gap: it names a
//! change-script step and a request line, and the step is applied (and
//! pushed) in the listener's middleware, before the handler that
//! serves that request runs.
//!
//! The `nth` knob picks which side of an id the change lands on: fire
//! before the page that CONTAINS the id, and both the feed and the
//! page offer it; fire before a LATER page, and the id was already
//! handed out when the feed changed it.
//!
//! ## The limit of this affordance
//!
//! Request granularity is the ceiling. A window that sits between two
//! operations INSIDE a consumer, with no intervening request, is not
//! addressable from here however the trigger is keyed - there is no
//! server interaction in that window to order against. Fixtures built
//! on this surface stage what the server does; the consumer's own
//! scheduling is still its own. Claiming otherwise in a fixture header
//! is worse than not having the fixture, because it makes a race look
//! covered.
//!
//! Every fire is recorded in the cross-protocol request log with
//! `announce: true`, immediately ahead of the request it preceded, so
//! a harness can assert the interleaving actually happened. That
//! matters more than it looks: a race gate whose trigger silently
//! never fired still runs its backfill and still passes, proving
//! nothing about the race it was written for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::shared::SharedHandles;
use crate::test_admin::{StepResult, advance_change_cursor};

/// Per-trigger match counts, keyed by the trigger's index in
/// `Fixture::announce`. Process-volatile - a trigger's "fire before
/// the 2nd matching request" is a property of this run, not of the
/// fixture image - and cleared by `POST /test/fixture/reset` so a
/// harness can replay the same script in one process.
pub type AnnounceCounters = Arc<Mutex<HashMap<usize, u32>>>;

/// Fire any trigger matching this request, BEFORE the handler runs.
///
/// Called from each HTTP listener's logging middleware, which holds no
/// fixture guard - the apply path takes the fixture write lock, so
/// calling this from anywhere that already holds a read guard would
/// deadlock.
///
/// `request_line` is `"<METHOD> <path>"` with no query string, the
/// same string the request log records, so a fixture author writes one
/// spelling for both.
pub fn fire_for_request(shared: &SharedHandles, protocol: &'static str, request_line: &str) {
    // Resolve which triggers match under a brief read guard and drop
    // it before applying anything.
    let matches: Vec<(usize, String, u32)> = {
        let fixture = shared.fixture.read().expect("fixture lock poisoned");
        if fixture.announce.is_empty() {
            return;
        }
        fixture
            .announce
            .iter()
            .enumerate()
            .filter(|(_, t)| request_line.starts_with(&t.before))
            .map(|(i, t)| (i, t.step.clone(), t.nth))
            .collect()
    };
    if matches.is_empty() {
        return;
    }

    // Count every match, fire only on the nth. Counting happens even
    // for triggers that will not fire this time, since that count is
    // what makes a later `nth` reachable.
    let mut due: Vec<(String, u32)> = Vec::new();
    {
        let mut counts = shared
            .announce_counts
            .lock()
            .expect("announce counter lock poisoned");
        for (index, step, nth) in matches {
            let seen = counts.entry(index).or_insert(0);
            *seen += 1;
            if *seen == nth {
                due.push((step, nth));
            }
        }
    }

    for (step, nth) in due {
        // Always the same apply path the step endpoint uses, gated on
        // the cursor being at the step the trigger names.
        let outcome = advance_change_cursor(shared, Some(&step));
        let detail = match &outcome {
            Ok(StepResult::Applied(applied)) => json!({
                "announce": true,
                "step": step,
                "nth": nth,
                "applied": true,
                "state": applied.new_state,
            }),
            // Both remaining cases are authoring faults, not runtime
            // conditions: the script ran out, or the cursor is not at
            // the step this trigger names (steps applied out of order,
            // or a step already consumed by an explicit
            // `POST /test/fixture/step`). Recorded loudly rather than
            // swallowed - a trigger that quietly did nothing is the
            // failure mode this whole surface exists to avoid.
            Ok(StepResult::Exhausted { .. }) => json!({
                "announce": true,
                "step": step,
                "nth": nth,
                "applied": false,
                "error": "change script is exhausted; the cursor is past its end",
            }),
            Err((code, payload)) => json!({
                "announce": true,
                "step": step,
                "nth": nth,
                "applied": false,
                "error": payload,
                "status": code.as_u16(),
            }),
        };
        shared
            .request_log
            .record(protocol, format!("ANNOUNCE {step}"), detail);
    }
}
