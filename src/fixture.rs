//! Fixture loader and validator.
//!
//! Reads a TOML fixture, normalises raw values into typed structs, and
//! enforces the invariants documented in `notes/fixture-format.md`. The
//! returned [`Fixture`] is read-only and feeds every JMAP response.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub state: String,
    pub account: Account,
    pub mailboxes: Vec<Mailbox>,
    pub emails: Vec<Email>,
    pub oauth: OAuthConfig,
    /// Calendars projected over the Microsoft Graph
    /// `/v1.0/me/calendars/...` surface (and, eventually, the
    /// CalDAV listener). Empty by default; fixtures that don't need
    /// calendar coverage can omit the `[[calendar]]` table.
    pub calendars: Vec<Calendar>,
    /// Events scoped to one of the declared calendars by
    /// `calendar_id`. Empty by default.
    pub events: Vec<Event>,
    /// Contact folders projected over the Graph
    /// `/v1.0/me/contactFolders/...` surface. Empty by default.
    pub contact_folders: Vec<ContactFolder>,
    /// Contacts scoped to a `ContactFolder` by `folder_id`. Empty by
    /// default.
    pub contacts: Vec<Contact>,
    /// Per-mutation transition log. Empty at load time (the seed
    /// state is the only known state); each successful `Email/set`
    /// or `Mailbox/set` envelope appends a transition and bumps
    /// `state`. JMAP `Email/changes` / `Mailbox/changes` walk this
    /// log to compute deltas between two known states.
    pub change_log: ChangeLog,
    /// Optional incremental-sync script. Populated by the Lua
    /// `change({...})` builder; empty for fixtures (TOML or Lua)
    /// that don't author any. The harness drives steps via
    /// `POST /test/fixture/step`; each step is applied atomically
    /// through `Fixture::mutate` so its full diff lands in one
    /// `Transition`. The script itself is read-only state on the
    /// fixture image and is restored on `POST /test/fixture/reset`
    /// alongside the rest of the baseline.
    pub change_script: Vec<ChangeStep>,
    /// Per-mailbox IMAP UID assignment history. Each entry is the
    /// insertion-ordered list of email ids ever assigned a UID in
    /// that mailbox. The Nth slot's UID is `N + 1`. Slots flip to
    /// `None` when an email is removed from the mailbox (delete,
    /// move out, expunge); slots are NEVER reclaimed, so existing
    /// UIDs stay put and `UIDNEXT` (= len + 1) only ever grows.
    /// This is the storage IMAP RFC 3501 §2.3.1.1 requires: a UID
    /// once assigned must never refer to a different message in
    /// the same `(UIDVALIDITY, mailbox)` pair, even after the
    /// original message is gone. Pre-fix the wire derived UIDs
    /// from filter-then-enumerate over the live email list, which
    /// silently reused UIDs after deletes / moves and let UIDNEXT
    /// shrink. JMAP / Graph / CalDAV don't read this field; it's
    /// IMAP-specific bookkeeping kept in the canonical Fixture so
    /// every mutation site updates exactly one place.
    pub mailbox_uid_history: std::collections::HashMap<String, Vec<Option<String>>>,
    /// Monotonic counter for synthesized `mock-event-N` ids
    /// produced by mutating handlers (JMAP `CalendarEvent/set` /
    /// gcal POST / Graph POST events). Initialised at load time
    /// to one past the highest `mock-event-N` already declared
    /// in the fixture, then incremented exactly once per mint via
    /// [`Self::mint_event_id`]. Never decremented or reset short
    /// of `/test/fixture/reset` (which restores the post-load
    /// baseline). Pre-fix every site used `events.len() + 1`,
    /// which collided with still-live ids after a destroy.
    pub synthetic_event_seq: u64,
    /// Monotonic counter for synthesized `mock-email-N` ids;
    /// same shape as `synthetic_event_seq`.
    pub synthetic_email_seq: u64,
}

/// Bounded ring of recent state transitions.
///
/// Each transition records the (oldState -> newState) pair and the
/// per-resource id sets the mutation touched. `Email/changes` walks
/// from the entry whose `from_state == sinceState` to the head and
/// unions the email-side ids; `Mailbox/changes` does the same for
/// the mailbox-side ids.
///
/// The ring is bounded at [`ChangeLog::MAX_TRANSITIONS`]. Once full,
/// the oldest transition is dropped on every push, which means a
/// `sinceState` that fell off the end now resolves to "unknown" and
/// the caller returns `cannotCalculateChanges`. Bounded retention
/// matches RFC 8620 §5.2: the server is free to forget how to compute
/// changes from arbitrarily-old states.
///
/// The seed state (the value of `Fixture::state` at load time) is
/// recorded separately and is *never* evicted: it always sits one
/// step before the first retained transition. A fresh
/// `sinceState == seed` therefore always resolves to the full delta
/// across every retained transition, regardless of how many later
/// mutations have rolled in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeLog {
    transitions: std::collections::VecDeque<Transition>,
    counter: u64,
    seed: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from_state: String,
    pub to_state: String,
    pub email_created: Vec<String>,
    pub email_updated: Vec<String>,
    pub email_destroyed: Vec<String>,
    pub mailbox_created: Vec<String>,
    pub mailbox_updated: Vec<String>,
    pub mailbox_destroyed: Vec<String>,
    pub event_created: Vec<String>,
    pub event_updated: Vec<String>,
    pub event_destroyed: Vec<String>,
    /// Parallel to `event_destroyed`: the `calendar_id` each
    /// destroyed event lived in. Captured at destroy time because
    /// the event is gone from the fixture by the time the delta
    /// walker reads it. Lets `event_delta_since` filter tombstones
    /// to a specific calendar so a per-calendar `calendarView/delta`
    /// doesn't surface tombstones for sibling calendars. Length
    /// must equal `event_destroyed`.
    pub event_destroyed_parents: Vec<String>,
    pub contact_created: Vec<String>,
    pub contact_updated: Vec<String>,
    pub contact_destroyed: Vec<String>,
    /// Parallel to `contact_destroyed`: the `folder_id` each
    /// destroyed contact lived in. Same role as
    /// `event_destroyed_parents`; lets folder-scoped
    /// `contacts/delta` filter tombstones.
    pub contact_destroyed_parents: Vec<String>,
    pub contact_folder_created: Vec<String>,
    pub contact_folder_updated: Vec<String>,
    pub contact_folder_destroyed: Vec<String>,
}

/// Resource-id deltas a single mutator pass produced. Returned by the
/// closure passed to [`Fixture::mutate`]; the caller never constructs
/// transitions directly. An all-empty `MutationDiff` is treated as a
/// no-op: the state token does not bump and no transition is recorded.
///
/// `event_destroyed_parents` / `contact_destroyed_parents` carry the
/// parent (calendar_id / folder_id) for each destroyed id so the
/// per-calendar / per-folder delta walkers can filter tombstones.
/// Producers must push to both vectors at the same time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutationDiff {
    pub email_created: Vec<String>,
    pub email_updated: Vec<String>,
    pub email_destroyed: Vec<String>,
    pub mailbox_created: Vec<String>,
    pub mailbox_updated: Vec<String>,
    pub mailbox_destroyed: Vec<String>,
    pub event_created: Vec<String>,
    pub event_updated: Vec<String>,
    pub event_destroyed: Vec<String>,
    pub event_destroyed_parents: Vec<String>,
    pub contact_created: Vec<String>,
    pub contact_updated: Vec<String>,
    pub contact_destroyed: Vec<String>,
    pub contact_destroyed_parents: Vec<String>,
    pub contact_folder_created: Vec<String>,
    pub contact_folder_updated: Vec<String>,
    pub contact_folder_destroyed: Vec<String>,
}

impl MutationDiff {
    pub fn is_empty(&self) -> bool {
        self.email_created.is_empty()
            && self.email_updated.is_empty()
            && self.email_destroyed.is_empty()
            && self.mailbox_created.is_empty()
            && self.mailbox_updated.is_empty()
            && self.mailbox_destroyed.is_empty()
            && self.event_created.is_empty()
            && self.event_updated.is_empty()
            && self.event_destroyed.is_empty()
            && self.contact_created.is_empty()
            && self.contact_updated.is_empty()
            && self.contact_destroyed.is_empty()
            && self.contact_folder_created.is_empty()
            && self.contact_folder_updated.is_empty()
            && self.contact_folder_destroyed.is_empty()
    }
}

impl ChangeLog {
    /// Bounded retention. Set to comfortably exceed the longest test
    /// scripts we run (the `lifecycle` and `scale` tests stay below
    /// 100 mutations); pick something high enough that an unrelated
    /// long-running fixture won't trip the eviction path mid-test
    /// but low enough that the per-mutation snapshot stays cheap.
    pub const MAX_TRANSITIONS: usize = 256;

    fn seed(seed_state: &str) -> Self {
        Self {
            transitions: std::collections::VecDeque::new(),
            counter: 0,
            seed: seed_state.to_string(),
        }
    }
}

impl Fixture {
    /// Borrowing iterator over the retained transitions, oldest
    /// first. CalDAV's per-resource ETag / CTag derivation walks
    /// this in reverse to find the last transition that touched
    /// a given event / calendar.
    pub fn change_log_transitions(
        &self,
    ) -> impl DoubleEndedIterator<Item = &Transition> {
        self.change_log.transitions.iter()
    }

    /// The change-log seed - the state token a fixture has after
    /// load, before any mutation. Used as the fallback CalDAV
    /// state value when no transition has touched a resource yet.
    pub fn change_log_seed(&self) -> &str {
        &self.change_log.seed
    }

    /// Read-only view of the per-mailbox UID history. Returns the
    /// empty slice for an unknown mailbox (no email has ever lived
    /// there).
    pub fn uid_history(&self, mailbox_id: &str) -> &[Option<String>] {
        self.mailbox_uid_history
            .get(mailbox_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Predicted next UID for `mailbox_id`. Equals
    /// `uid_history.len() + 1`; monotonically increasing across
    /// the fixture's lifetime regardless of deletes / moves.
    pub fn uidnext(&self, mailbox_id: &str) -> u32 {
        u32::try_from(self.uid_history(mailbox_id).len() + 1).expect("uidnext fits in u32")
    }

    /// Allocate the next UID slot in `mailbox_id` for `email_id`.
    /// Pushes onto the history (slot index = uid - 1) and returns
    /// the assigned UID. Callers must invoke this whenever an
    /// email gains membership in a mailbox: load-time declaration,
    /// JMAP `Email/set` create / mailboxIds add, change-script
    /// `EmailCreate` / `EmailMove`, IMAP `UID COPY`.
    pub fn assign_uid(&mut self, mailbox_id: &str, email_id: String) -> u32 {
        let history = self
            .mailbox_uid_history
            .entry(mailbox_id.to_string())
            .or_default();
        history.push(Some(email_id));
        u32::try_from(history.len()).expect("uid fits in u32")
    }

    /// Mark the slot holding `email_id` in `mailbox_id` as retired
    /// (`None`). Idempotent: missing email or mailbox is a no-op.
    /// A given email is in a mailbox at most once, so the first
    /// matching slot wins. The slot is never reclaimed; UIDs
    /// past it stay assigned.
    pub fn retire_uid(&mut self, mailbox_id: &str, email_id: &str) {
        if let Some(history) = self.mailbox_uid_history.get_mut(mailbox_id) {
            for slot in history.iter_mut() {
                if slot.as_deref() == Some(email_id) {
                    *slot = None;
                    break;
                }
            }
        }
    }

    /// Sync per-mailbox UID assignments after an in-place edit to
    /// `email.mailbox_ids`: assign UIDs in newly-joined mailboxes
    /// and retire UIDs in newly-left ones. Handy for the JMAP /
    /// change-script paths that apply a JSON patch and don't track
    /// the membership diff explicitly.
    pub fn sync_mailbox_uids(
        &mut self,
        email_id: &str,
        old_mailboxes: &[String],
        new_mailboxes: &[String],
    ) {
        for old in old_mailboxes {
            if !new_mailboxes.iter().any(|n| n == old) {
                self.retire_uid(old, email_id);
            }
        }
        for new in new_mailboxes {
            if !old_mailboxes.iter().any(|o| o == new) {
                self.assign_uid(new, email_id.to_string());
            }
        }
    }

    /// Rebuild `mailbox_uid_history` from the current `emails`
    /// list, treating each email's membership in each mailbox as a
    /// fresh load-time declaration. Used by hand-built test
    /// fixtures (the canonical loader does this in
    /// `normalize_with_dir`); not for production mutation paths,
    /// which must use `assign_uid` / `retire_uid` so existing UIDs
    /// stay stable.
    #[doc(hidden)]
    pub fn rebuild_uid_history(&mut self) {
        self.mailbox_uid_history.clear();
        for email in &self.emails {
            for mb in &email.mailbox_ids {
                self.mailbox_uid_history
                    .entry(mb.clone())
                    .or_default()
                    .push(Some(email.id.clone()));
            }
        }
    }
}

/// Fold the per-transition diffs over the half-open range
/// `(from, to]` (i.e. every transition whose `from_state == from` or
/// later, up to `to`). Applies RFC 8620 §5.2 dominance: an id that
/// was both created and destroyed in the window is omitted entirely
/// (the caller never knew it existed); created+updated collapses to
/// created; destroyed+updated collapses to destroyed.
#[derive(Debug, Default)]
pub struct DeltaSet {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
}

impl Fixture {
    /// Mint a fresh synthesized event id of the form
    /// `mock-event-N`. The counter advances monotonically across
    /// the fixture lifetime; never reuses an id even after the
    /// underlying event is destroyed. Reset only via
    /// `/test/fixture/reset` (which restores the post-load
    /// baseline).
    pub fn mint_event_id(&mut self) -> String {
        self.synthetic_event_seq += 1;
        format!("mock-event-{}", self.synthetic_event_seq)
    }

    /// Email-side analogue of [`Self::mint_event_id`].
    pub fn mint_email_id(&mut self) -> String {
        self.synthetic_email_seq += 1;
        format!("mock-email-{}", self.synthetic_email_seq)
    }

    /// Apply a mutation, record its transition, and bump `state`.
    /// The closure is the only thing allowed to touch the fixture
    /// fields; it returns the resource-id diff so we can capture it
    /// without re-walking the (potentially large) email/mailbox
    /// vectors. An all-empty diff is a no-op (no state bump, no
    /// transition recorded) so that idempotent set-calls (e.g. an
    /// `update` block that only patches keywords already present)
    /// stay observable as "nothing changed".
    ///
    /// Most callers want this shape. The change-script step path
    /// in `routes.rs` already mutates the fixture in place across
    /// many ops and only needs to record the cumulative transition
    /// at the end; that path uses [`Self::record_transition`]
    /// directly.
    pub fn mutate<F>(&mut self, f: F) -> Transition
    where
        F: FnOnce(&mut Fixture) -> MutationDiff,
    {
        let diff = f(self);
        self.record_transition(diff)
    }

    /// Record a `MutationDiff` against the current fixture state:
    /// bump `state`, append a `Transition` to the change_log, and
    /// return the recorded transition. Public so callers that have
    /// already mutated the fixture in place (the change-script step
    /// applier in particular) can capture the cumulative diff
    /// without going through `mutate`'s closure dance.
    ///
    /// All-empty diff stays a no-op (no state bump, no transition
    /// appended); the returned transition has `from == to` and
    /// empty id sets so callers reporting `oldState` / `newState`
    /// see "nothing changed".
    pub fn record_transition(&mut self, diff: MutationDiff) -> Transition {
        let from_state = self.state.clone();
        if diff.is_empty() {
            return Transition {
                from_state: from_state.clone(),
                to_state: from_state,
                email_created: vec![],
                email_updated: vec![],
                email_destroyed: vec![],
                mailbox_created: vec![],
                mailbox_updated: vec![],
                mailbox_destroyed: vec![],
                event_created: vec![],
                event_updated: vec![],
                event_destroyed: vec![],
                event_destroyed_parents: vec![],
                contact_created: vec![],
                contact_updated: vec![],
                contact_destroyed: vec![],
                contact_destroyed_parents: vec![],
                contact_folder_created: vec![],
                contact_folder_updated: vec![],
                contact_folder_destroyed: vec![],
            };
        }
        self.change_log.counter += 1;
        let to_state = format!("{}.{}", self.change_log.seed, self.change_log.counter);
        self.state = to_state.clone();
        let trans = Transition {
            from_state,
            to_state,
            email_created: diff.email_created,
            email_updated: diff.email_updated,
            email_destroyed: diff.email_destroyed,
            mailbox_created: diff.mailbox_created,
            mailbox_updated: diff.mailbox_updated,
            mailbox_destroyed: diff.mailbox_destroyed,
            event_created: diff.event_created,
            event_updated: diff.event_updated,
            event_destroyed: diff.event_destroyed,
            event_destroyed_parents: diff.event_destroyed_parents,
            contact_created: diff.contact_created,
            contact_updated: diff.contact_updated,
            contact_destroyed: diff.contact_destroyed,
            contact_destroyed_parents: diff.contact_destroyed_parents,
            contact_folder_created: diff.contact_folder_created,
            contact_folder_updated: diff.contact_folder_updated,
            contact_folder_destroyed: diff.contact_folder_destroyed,
        };
        debug_assert_eq!(
            trans.event_destroyed.len(),
            trans.event_destroyed_parents.len(),
            "event_destroyed_parents must be parallel to event_destroyed"
        );
        debug_assert_eq!(
            trans.contact_destroyed.len(),
            trans.contact_destroyed_parents.len(),
            "contact_destroyed_parents must be parallel to contact_destroyed"
        );
        if self.change_log.transitions.len() >= ChangeLog::MAX_TRANSITIONS {
            self.change_log.transitions.pop_front();
        }
        self.change_log.transitions.push_back(trans.clone());
        trans
    }

    /// Compute the email-side delta between two known states.
    ///
    /// - `sinceState == self.state`: empty delta.
    /// - `sinceState` matches the seed or any retained transition's
    ///   `from_state`: walk forward from there, unioning with RFC
    ///   §5.2 dominance.
    /// - `sinceState` is unknown (older than seed, or evicted from
    ///   the bounded ring, or simply not a state we ever issued):
    ///   returns `None`, which the caller maps to
    ///   `cannotCalculateChanges`.
    pub fn email_delta_since(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (
                &t.email_created,
                &t.email_updated,
                &t.email_destroyed,
            )
        })
    }

    /// Mailbox-side analogue of [`email_delta_since`].
    pub fn mailbox_delta_since(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (
                &t.mailbox_created,
                &t.mailbox_updated,
                &t.mailbox_destroyed,
            )
        })
    }

    /// Event-side analogue of [`email_delta_since`]. Drives the
    /// Microsoft Graph `calendarView/delta` and `events/delta`
    /// surfaces: a follow-up call with a known `$deltatoken` returns
    /// only the events that changed since that token, plus a fresh
    /// deltaLink. Tokens older than the seed (or evicted from the
    /// bounded ring) return `None`; the Graph layer converts that to
    /// a full re-bootstrap.
    ///
    /// All three sets are scoped to the named calendar:
    /// - Destroyed ids are filtered against `event_destroyed_parents`
    ///   recorded at retire time (the event is gone from the live
    ///   fixture by then).
    /// - Created / updated ids are filtered against the live event's
    ///   current `calendar_id`. An event whose every transition in
    ///   the window was on a sibling calendar is dropped here so
    ///   the wire delta does not over-report. (Pre-fix this filter
    ///   was the caller's responsibility, but the JMAP `Calendar
    ///   Event/changes` cross-calendar union has no good way to
    ///   apply it; folding the filter here keeps every consumer
    ///   honest.)
    pub fn event_delta_since(&self, since: &str, calendar_id: &str) -> Option<DeltaSet> {
        let mut delta = self.delta_since_filtered_destroys(
            since,
            |t| (&t.event_created, &t.event_updated, &t.event_destroyed),
            |t| Some(&t.event_destroyed_parents),
            calendar_id,
        )?;
        let live: std::collections::HashMap<&str, &str> = self
            .events
            .iter()
            .map(|e| (e.id.as_str(), e.calendar_id.as_str()))
            .collect();
        delta
            .created
            .retain(|id| live.get(id.as_str()).copied() == Some(calendar_id));
        delta
            .updated
            .retain(|id| live.get(id.as_str()).copied() == Some(calendar_id));
        Some(delta)
    }

    /// Cross-calendar event delta. Returns every event change since
    /// `since` regardless of which calendar it lives in. Drives
    /// JMAP `CalendarEvent/changes`, which carries no per-calendar
    /// filter on the wire (the `calendarIds` map on each event in
    /// the follow-up `CalendarEvent/get` is the per-resource scope).
    pub fn event_delta_since_any(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (
                &t.event_created,
                &t.event_updated,
                &t.event_destroyed,
            )
        })
    }

    /// Contact-side analogue. Drives the Graph
    /// `/v1.0/me/contactFolders/{id}/contacts/delta` surface: a
    /// follow-up call with a known `$deltatoken` returns only the
    /// contacts that changed since that token. Tokens older than
    /// the seed (or evicted from the bounded ring) return `None`;
    /// the Graph layer translates that to a 410 Gone (which
    /// ratatoskr handles by re-bootstrapping with a full sync).
    ///
    /// Same parent-filtering shape as `event_delta_since`: tombstones
    /// are scoped to the named folder via `contact_destroyed_parents`.
    pub fn contact_delta_since(&self, since: &str, folder_id: &str) -> Option<DeltaSet> {
        self.delta_since_filtered_destroys(
            since,
            |t| {
                (
                    &t.contact_created,
                    &t.contact_updated,
                    &t.contact_destroyed,
                )
            },
            |t| Some(&t.contact_destroyed_parents),
            folder_id,
        )
    }

    /// Cross-folder contact delta. Returns every contact change
    /// since `since` regardless of folder. Drives the People API
    /// listener, which has no folder concept (Google flattens
    /// every account contact into one connections list).
    pub fn contact_delta_since_any(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (
                &t.contact_created,
                &t.contact_updated,
                &t.contact_destroyed,
            )
        })
    }

    /// Contact-folder-side analogue.
    pub fn contact_folder_delta_since(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (
                &t.contact_folder_created,
                &t.contact_folder_updated,
                &t.contact_folder_destroyed,
            )
        })
    }

    /// Like [`Self::delta_since`], but the destroyed walk is filtered
    /// by a parent id (calendar / folder). For each transition the
    /// `parents` projector returns a `Vec<String>` parallel to the
    /// destroyed list; only destroyed ids whose corresponding parent
    /// matches `parent_id` are accumulated.
    fn delta_since_filtered_destroys<'a, F, G>(
        &'a self,
        since: &str,
        project: F,
        parents: G,
        parent_id: &str,
    ) -> Option<DeltaSet>
    where
        F: Fn(&'a Transition) -> (&'a Vec<String>, &'a Vec<String>, &'a Vec<String>),
        G: Fn(&'a Transition) -> Option<&'a Vec<String>>,
    {
        if since == self.state {
            return Some(DeltaSet::default());
        }
        let seed_known = since == self.change_log.seed;
        let head_known = self
            .change_log
            .transitions
            .iter()
            .any(|t| t.from_state == since);
        if !seed_known && !head_known {
            return None;
        }
        let mut started = seed_known;
        let mut created: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let mut destroyed: Vec<String> = Vec::new();
        for t in &self.change_log.transitions {
            if !started {
                if t.from_state == since {
                    started = true;
                } else {
                    continue;
                }
            }
            let (c, u, d) = project(t);
            created.extend(c.iter().cloned());
            updated.extend(u.iter().cloned());
            // Only keep destroyed ids whose parent is the requested
            // one. Producers must keep the parents vec parallel to
            // the destroyed vec; if it's missing entirely (legacy /
            // empty transition) skip.
            if let Some(p) = parents(t)
                && p.len() == d.len()
            {
                for (id, parent) in d.iter().zip(p.iter()) {
                    if parent == parent_id {
                        destroyed.push(id.clone());
                    }
                }
            }
        }
        apply_dominance_and_dedup(&mut created, &mut updated, &mut destroyed);
        Some(DeltaSet {
            created,
            updated,
            destroyed,
        })
    }

    fn delta_since<'a, F>(&'a self, since: &str, project: F) -> Option<DeltaSet>
    where
        F: Fn(&'a Transition) -> (&'a Vec<String>, &'a Vec<String>, &'a Vec<String>),
    {
        if since == self.state {
            return Some(DeltaSet::default());
        }
        // The seed and every retained `from_state` count as "known".
        // If `since` matches none of them, we can no longer reconstruct
        // the delta and the caller returns `cannotCalculateChanges`.
        let seed_known = since == self.change_log.seed;
        let head_known = self
            .change_log
            .transitions
            .iter()
            .any(|t| t.from_state == since);
        if !seed_known && !head_known {
            return None;
        }
        let mut started = seed_known;
        let mut created: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let mut destroyed: Vec<String> = Vec::new();
        for t in &self.change_log.transitions {
            if !started {
                if t.from_state == since {
                    started = true;
                } else {
                    continue;
                }
            }
            let (c, u, d) = project(t);
            created.extend(c.iter().cloned());
            updated.extend(u.iter().cloned());
            destroyed.extend(d.iter().cloned());
        }
        apply_dominance_and_dedup(&mut created, &mut updated, &mut destroyed);
        Some(DeltaSet {
            created,
            updated,
            destroyed,
        })
    }
}

/// Apply RFC 8620 §5.2 dominance to a freshly-extended delta and
/// dedup each list while preserving first-seen order (stable for
/// byte-determinism). Shared by both delta walkers in `Fixture`.
///
/// Dominance rules (order matters):
/// 1. `created ∩ destroyed` cancels (both removed).
/// 2. `updated` is dropped where the id also appears in the
///    surviving created or destroyed list.
///
/// Pre-fix the cancel filter scanned `destroyed` linearly per
/// `created` id (`destroyed.contains(id)` over a `Vec`), going
/// O(c·d) on a stale-client delta walk. Now the membership probes
/// hash on `&str`.
fn apply_dominance_and_dedup(
    created: &mut Vec<String>,
    updated: &mut Vec<String>,
    destroyed: &mut Vec<String>,
) {
    use std::collections::HashSet;
    // Cancel set: only need `&str` membership against destroyed,
    // and the filtered ids land back in an owned `HashSet<String>`
    // for the subsequent retain calls (which can't borrow from
    // either vec while mutating).
    let cancel: HashSet<String> = {
        let destroyed_set: HashSet<&str> = destroyed.iter().map(String::as_str).collect();
        created
            .iter()
            .filter(|id| destroyed_set.contains(id.as_str()))
            .cloned()
            .collect()
    };
    created.retain(|id| !cancel.contains(id));
    destroyed.retain(|id| !cancel.contains(id));
    // updated.retain reads `created` / `destroyed` immutably; safe
    // to borrow them as `&str` here.
    let in_created: HashSet<&str> = created.iter().map(String::as_str).collect();
    let in_destroyed: HashSet<&str> = destroyed.iter().map(String::as_str).collect();
    updated.retain(|id| !in_created.contains(id.as_str()) && !in_destroyed.contains(id.as_str()));
    dedup_preserving_order(created);
    dedup_preserving_order(updated);
    dedup_preserving_order(destroyed);
}

fn dedup_preserving_order(v: &mut Vec<String>) {
    if v.len() < 2 {
        return;
    }
    // Two passes: first builds a keep-mask using `&str` membership
    // (no clones); then `retain` drops dropped slots in O(n).
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(v.len());
    let keep: Vec<bool> = v.iter().map(|s| seen.insert(s.as_str())).collect();
    let mut idx = 0;
    v.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

/// One named entry in the incremental-sync script. Authored via
/// the Lua `change({ id = "...", ... })` builder. Steps are
/// applied atomically: the handler accumulates every op's resource
/// touch into one `MutationDiff` and routes it through a single
/// `Fixture::mutate` call, so the change_log gains exactly one
/// `Transition` per step. That keeps `Email/changes` collapse rules
/// natural - within-step create+destroy cancels, create+update
/// collapses, etc. - even when a step does several things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeStep {
    pub id: String,
    pub ops: Vec<ChangeOp>,
}

/// A single mutation in a [`ChangeStep`]. Patches use the same wire
/// shape as JMAP `Email/set` / `Mailbox/set` (`keywords` /
/// `keywords/<flag>`, `mailboxIds` / `mailboxIds/<id>`, plus the
/// mailbox metadata properties), so the step handler can route them
/// through `crate::jmap::apply_email_patch` /
/// `crate::jmap::apply_mailbox_patch` rather than reimplementing.
///
/// `EmailMove` is a convenience alias for "update the email's
/// `mailboxIds` to this exact set". It applies as a regular update
/// (the diff lands in `email_updated`) but the step handler also
/// surfaces it under `changes.emails.moved` in the response so test
/// harnesses can distinguish a move from a flag flip without
/// re-walking the resulting state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOp {
    EmailCreate(Box<Email>),
    EmailUpdate {
        id: String,
        patch: serde_json::Value,
    },
    EmailMove {
        id: String,
        mailbox_ids: Vec<String>,
    },
    EmailDestroy {
        id: String,
    },
    MailboxCreate(Box<Mailbox>),
    MailboxUpdate {
        id: String,
        patch: serde_json::Value,
    },
    MailboxDestroy {
        id: String,
    },
    EventCreate(Box<Event>),
    EventUpdate {
        id: String,
        patch: serde_json::Value,
    },
    EventDestroy {
        id: String,
    },
    ContactFolderCreate(Box<ContactFolder>),
    ContactFolderUpdate {
        id: String,
        patch: serde_json::Value,
    },
    ContactFolderDestroy {
        id: String,
    },
    ContactCreate(Box<Contact>),
    ContactUpdate {
        id: String,
        patch: serde_json::Value,
    },
    ContactDestroy {
        id: String,
    },
}

/// Fixture-side OAuth configuration. Optional in TOML/Lua; defaults
/// to `enforce = false` so existing fixtures keep behaving like the
/// "no auth in v0" baseline. When `enforce = true`, the JMAP /
/// Graph / Gmail HTTP listeners reject requests whose
/// `Authorization: Bearer <token>` is not in the active token set
/// (managed by `crate::oauth::TokenStore`). IMAP and SMTP have
/// their own auth surfaces and are unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    pub enforce: bool,
    /// Issuer string echoed back in `userinfo` responses. Most
    /// fixtures don't care; the default keeps userinfo
    /// self-consistent without a fixture-side decision.
    pub issuer: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            enforce: false,
            issuer: "https://saehrimnir.test/oauth".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    /// Echoed as `color` in Graph responses. Real Graph uses the
    /// enum names `lightBlue` / `lightGreen` / etc.; we accept any
    /// string and pass it through.
    pub color: Option<String>,
    /// At most one calendar per fixture may have
    /// `is_default = true`; the loader rejects fixtures with
    /// multiple defaults.
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub calendar_id: String,
    pub subject: String,
    pub body_preview: Option<String>,
    pub body_text: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: Option<String>,
    pub organizer: Option<Address>,
    pub attendees: Vec<Address>,
    pub is_all_day: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactFolder {
    pub id: String,
    pub display_name: String,
    /// Optional parent for nested folders. Graph supports nesting via
    /// `/v1.0/me/contactFolders/{id}/childFolders`; v0 fixtures stay
    /// flat, but the field is here so future scenarios can grow into
    /// it without a schema change.
    pub parent_folder_id: Option<String>,
    /// At most one folder per fixture may be `is_default = true`. The
    /// loader rejects fixtures with multiple defaults; the Graph
    /// layer surfaces it as the canonical "Contacts" folder a fresh
    /// Outlook profile creates by default.
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub folder_id: String,
    pub display_name: Option<String>,
    pub emails: Vec<ContactEmail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactEmail {
    pub address: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub role: Option<Role>,
    pub parent_id: Option<String>,
    pub sort_order: Option<i64>,
    pub is_subscribed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Inbox,
    Archive,
    Drafts,
    Sent,
    Trash,
    Junk,
    Important,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Archive => "archive",
            Self::Drafts => "drafts",
            Self::Sent => "sent",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Important => "important",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "inbox" => Ok(Self::Inbox),
            "archive" => Ok(Self::Archive),
            "drafts" => Ok(Self::Drafts),
            "sent" => Ok(Self::Sent),
            "trash" => Ok(Self::Trash),
            "junk" => Ok(Self::Junk),
            "important" => Ok(Self::Important),
            other => Err(format!("unknown role {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub id: String,
    pub thread_id: String,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
    pub size: i64,
    pub received_at: DateTime<Utc>,
    pub sent_at: DateTime<Utc>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: Option<String>,
    pub preview: Option<String>,
    pub message_id: Vec<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub has_attachment: bool,
    pub body: Body,
    pub attachments: Vec<Attachment>,
    /// IMAP-wire override. When `Some`, the IMAP layer emits these
    /// bytes verbatim for `BODY[]` / `RFC822.SIZE` and slices them
    /// for `BODY[HEADER]` / `BODY[TEXT]` instead of composing from
    /// the canonical headers + `body` + `attachments`. Lets fixtures
    /// hand-author malformed MIME (broken boundaries, non-canonical
    /// header layouts, encoded-word edge cases) for client-tolerance
    /// tests. JMAP / Gmail / Graph projections ignore this field and
    /// keep reading from the structured fields, so a fixture that
    /// wants useful content on the other protocols sets `body_text`
    /// alongside; a pure-IMAP adversarial fixture can leave
    /// `body_text` minimal. Mutually exclusive with `attachments`
    /// (the raw bytes are the entire body, including any MIME
    /// structure the author wanted).
    pub raw_bytes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Stable opaque blob identifier referenced from the JMAP
    /// `attachments[]` `blobId` field, the Gmail `attachmentId`, and
    /// the Graph `id` of an attachment resource.
    pub blob_id: String,
    pub name: String,
    pub content_type: String,
    /// Defaults to `data.len()` if the fixture omits it. Wire layers
    /// emit this value verbatim - the determinism contract requires
    /// it match what each protocol decodes.
    pub size: i64,
    pub disposition: Disposition,
    /// Content-ID for inline parts (without angle brackets - those
    /// are added per protocol on emit).
    pub cid: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Attachment,
    Inline,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Inline => "inline",
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "attachment" => Ok(Self::Attachment),
            "inline" => Ok(Self::Inline),
            other => Err(format!("unknown disposition {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Inline plain-text body. The default for v0 fixtures.
    Text(String),
}

// ── Raw types for serde deserialization ─────────────────────────────
//
// These also serve as the in-memory shape the Lua loader builds up
// before handing off to `normalize` for cross-reference validation.

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawFixture {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) state: Option<String>,
    pub(crate) account: RawAccount,
    #[serde(default, rename = "mailbox")]
    pub(crate) mailboxes: Vec<RawMailbox>,
    #[serde(default, rename = "email")]
    pub(crate) emails: Vec<RawEmail>,
    #[serde(default)]
    pub(crate) oauth: Option<RawOAuth>,
    #[serde(default, rename = "calendar")]
    pub(crate) calendars: Vec<RawCalendar>,
    #[serde(default, rename = "event")]
    pub(crate) events: Vec<RawEvent>,
    #[serde(default, rename = "contact_folder")]
    pub(crate) contact_folders: Vec<RawContactFolder>,
    #[serde(default, rename = "contact")]
    pub(crate) contacts: Vec<RawContact>,
    #[serde(default, rename = "change")]
    pub(crate) change_script: Vec<RawChangeStep>,
}

/// One named step in the TOML projection of the change-script
/// surface. Each bucket mirrors the Lua `change({...})` field with
/// the same name; every bucket is optional so a step that names just
/// one op kind is valid TOML.
///
/// The op order inside the resulting [`ChangeStep`] matches the Lua
/// reader: email_create / email_update / email_move / email_destroy,
/// then mailbox, then event, then contact_folder, then contact. The
/// step handler walks ops in the produced order, so this also fixes
/// the apply order (mailbox_create runs before email_create that
/// references it within the same step, etc.).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawChangeStep {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) email_create: Vec<RawEmail>,
    #[serde(default)]
    pub(crate) email_update: Vec<RawEmailUpdate>,
    #[serde(default)]
    pub(crate) email_move: Vec<RawEmailMove>,
    #[serde(default)]
    pub(crate) email_destroy: Vec<String>,
    #[serde(default)]
    pub(crate) mailbox_create: Vec<RawMailbox>,
    #[serde(default)]
    pub(crate) mailbox_update: Vec<RawMailboxUpdate>,
    #[serde(default)]
    pub(crate) mailbox_destroy: Vec<String>,
    #[serde(default)]
    pub(crate) event_create: Vec<RawEvent>,
    #[serde(default)]
    pub(crate) event_update: Vec<RawEventUpdate>,
    #[serde(default)]
    pub(crate) event_destroy: Vec<String>,
    #[serde(default)]
    pub(crate) contact_folder_create: Vec<RawContactFolder>,
    #[serde(default)]
    pub(crate) contact_folder_update: Vec<RawContactFolderUpdate>,
    #[serde(default)]
    pub(crate) contact_folder_destroy: Vec<String>,
    #[serde(default)]
    pub(crate) contact_create: Vec<RawContact>,
    #[serde(default)]
    pub(crate) contact_update: Vec<RawContactUpdate>,
    #[serde(default)]
    pub(crate) contact_destroy: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawEmailUpdate {
    pub(crate) id: String,
    /// Full-replace keyword set; `Some(vec![])` clears all flags.
    /// Absent leaves keywords untouched.
    #[serde(default)]
    pub(crate) keywords: Option<Vec<String>>,
    /// Full-replace mailbox membership.
    #[serde(default)]
    pub(crate) mailbox_ids: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawEmailMove {
    pub(crate) id: String,
    pub(crate) mailbox_ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawMailboxUpdate {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) parent_id: Option<String>,
    #[serde(default)]
    pub(crate) sort_order: Option<i64>,
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) is_subscribed: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawEventUpdate {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) start: Option<String>,
    #[serde(default)]
    pub(crate) end: Option<String>,
    #[serde(default)]
    pub(crate) location: Option<String>,
    #[serde(default)]
    pub(crate) body_text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawContactFolderUpdate {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) parent_folder_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawContactUpdate {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) folder_id: Option<String>,
    /// Full-replace email list when present.
    #[serde(default)]
    pub(crate) emails: Option<Vec<RawContactEmail>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawContactFolder {
    pub(crate) id: String,
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) parent_folder_id: Option<String>,
    #[serde(default)]
    pub(crate) is_default: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawContact {
    pub(crate) id: String,
    pub(crate) folder_id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) emails: Vec<RawContactEmail>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawContactEmail {
    /// Bare address string. Identical sugar to the `email` field on
    /// emails / events: keeps simple fixtures terse.
    Bare(String),
    Full {
        address: String,
        #[serde(default)]
        name: Option<String>,
    },
}

impl From<RawContactEmail> for ContactEmail {
    fn from(raw: RawContactEmail) -> Self {
        match raw {
            RawContactEmail::Bare(address) => Self { address, name: None },
            RawContactEmail::Full { address, name } => Self { address, name },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawCalendar {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) color: Option<String>,
    #[serde(default)]
    pub(crate) is_default: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawEvent {
    pub(crate) id: String,
    pub(crate) calendar_id: String,
    pub(crate) subject: String,
    #[serde(default)]
    pub(crate) body_preview: Option<String>,
    #[serde(default)]
    pub(crate) body_text: Option<String>,
    pub(crate) start: String,
    pub(crate) end: String,
    #[serde(default)]
    pub(crate) location: Option<String>,
    #[serde(default)]
    pub(crate) organizer: Option<RawAddress>,
    #[serde(default)]
    pub(crate) attendees: Vec<RawAddress>,
    #[serde(default)]
    pub(crate) is_all_day: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawOAuth {
    #[serde(default)]
    pub(crate) enforce: bool,
    #[serde(default)]
    pub(crate) issuer: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawAccount {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) is_personal: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawMailbox {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) parent_id: Option<String>,
    #[serde(default)]
    pub(crate) sort_order: Option<i64>,
    #[serde(default)]
    pub(crate) is_subscribed: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawEmail {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    pub(crate) mailbox_ids: Vec<String>,
    #[serde(default)]
    pub(crate) keywords: Vec<String>,
    #[serde(default)]
    pub(crate) size: Option<i64>,
    pub(crate) received_at: String,
    #[serde(default)]
    pub(crate) sent_at: Option<String>,
    #[serde(default)]
    pub(crate) from: Option<RawAddress>,
    #[serde(default)]
    pub(crate) to: Vec<RawAddress>,
    #[serde(default)]
    pub(crate) cc: Vec<RawAddress>,
    #[serde(default)]
    pub(crate) bcc: Vec<RawAddress>,
    #[serde(default)]
    pub(crate) reply_to: Vec<RawAddress>,
    #[serde(default)]
    pub(crate) subject: Option<String>,
    #[serde(default)]
    pub(crate) preview: Option<String>,
    #[serde(default)]
    pub(crate) message_id: Vec<String>,
    #[serde(default)]
    pub(crate) in_reply_to: Vec<String>,
    #[serde(default)]
    pub(crate) references: Vec<String>,
    #[serde(default)]
    pub(crate) has_attachment: Option<bool>,
    #[serde(default)]
    pub(crate) body_text: Option<String>,
    /// IMAP-wire raw-bytes override. See [`Email::raw_bytes`].
    #[serde(default)]
    pub(crate) body_raw_bytes: Option<String>,
    #[serde(default, rename = "attachment")]
    pub(crate) attachments: Vec<RawAttachment>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawAttachment {
    pub(crate) blob_id: String,
    pub(crate) name: String,
    pub(crate) content_type: String,
    #[serde(default)]
    pub(crate) size: Option<i64>,
    #[serde(default)]
    pub(crate) disposition: Option<String>,
    #[serde(default)]
    pub(crate) cid: Option<String>,
    /// Path to the blob bytes, resolved relative to the fixture file's
    /// parent directory.
    pub(crate) data_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawAddress {
    Bare(String),
    Full {
        #[serde(default)]
        name: Option<String>,
        email: String,
    },
}

impl From<RawAddress> for Address {
    fn from(raw: RawAddress) -> Self {
        match raw {
            RawAddress::Bare(email) => Self { name: None, email },
            RawAddress::Full { name, email } => Self { name, email },
        }
    }
}

// ── Loader ──────────────────────────────────────────────────────────

/// Public entry point. Dispatches by extension: `.lua` files go through
/// the dellingr-backed scenario loader; everything else is parsed as
/// TOML. Attachment `data_path` references resolve relative to the
/// fixture file's parent directory.
pub fn load(path: &Path) -> Result<Fixture, String> {
    if path.extension().is_some_and(|e| e == "lua") {
        crate::lua::load(path)
    } else {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw: RawFixture =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        normalize_with_dir(raw, dir)
    }
}

/// Convenience wrapper for unit tests that build a `RawFixture` from
/// inline strings without a fixture file on disk. Resolves any
/// `data_path` references relative to the current working directory;
/// fixture-file loading goes through [`normalize_with_dir`].
#[cfg(test)]
pub(crate) fn normalize(raw: RawFixture) -> Result<Fixture, String> {
    normalize_with_dir(raw, Path::new("."))
}

pub(crate) fn normalize_with_dir(raw: RawFixture, fixture_dir: &Path) -> Result<Fixture, String> {
    if !raw.account.is_personal {
        return Err("account.is_personal must be true (v0 supports one personal account)".into());
    }
    // OAuth's userinfo endpoint serves `account.name` verbatim as
    // both the `email` and `name` claims. Reject non-email-shaped
    // names at load time so a fixture can't ship a misleading
    // `email` claim - cheaper to fail loud here than to chase
    // down a downstream client confused by `email: "Display
    // Name"`. The check is intentionally minimal (one `@`, non-
    // empty local, dotted domain, no whitespace); fixtures that
    // need richer mailbox-name semantics can grow a separate
    // `account.email` field later.
    if !is_email_shaped(&raw.account.name) {
        return Err(format!(
            "account.name must be email-shaped (got {:?}); the OAuth userinfo endpoint exposes it as the `email` claim",
            raw.account.name
        ));
    }

    let mut mb_ids: HashMap<String, ()> = HashMap::new();
    for mb in &raw.mailboxes {
        if mb_ids.insert(mb.id.clone(), ()).is_some() {
            return Err(format!("duplicate mailbox id {:?}", mb.id));
        }
    }

    let mut mailboxes = Vec::with_capacity(raw.mailboxes.len());
    for mb in raw.mailboxes {
        let role = mb.role.as_deref().map(Role::parse).transpose()?;
        if let Some(parent) = &mb.parent_id
            && !mb_ids.contains_key(parent)
        {
            return Err(format!(
                "mailbox {:?}: parent_id {parent:?} does not exist",
                mb.id
            ));
        }
        mailboxes.push(Mailbox {
            is_subscribed: mb.is_subscribed.unwrap_or(true),
            id: mb.id,
            name: mb.name,
            role,
            parent_id: mb.parent_id,
            sort_order: mb.sort_order,
        });
    }
    detect_cycles(&mailboxes)?;

    let mut email_ids: HashMap<String, ()> = HashMap::new();
    for em in &raw.emails {
        if email_ids.insert(em.id.clone(), ()).is_some() {
            return Err(format!("duplicate email id {:?}", em.id));
        }
    }

    let mut emails = Vec::with_capacity(raw.emails.len());
    for em in raw.emails {
        let email = normalize_email(em, &mb_ids, fixture_dir)?;
        emails.push(email);
    }

    let oauth = match raw.oauth {
        Some(raw_oauth) => {
            let default = OAuthConfig::default();
            OAuthConfig {
                enforce: raw_oauth.enforce,
                issuer: raw_oauth.issuer.unwrap_or(default.issuer),
            }
        }
        None => OAuthConfig::default(),
    };

    // Calendars and events. Validate that every event references a
    // declared calendar - same shape as the email -> mailbox check.
    let mut calendar_ids: HashMap<String, ()> = HashMap::new();
    let mut calendars = Vec::with_capacity(raw.calendars.len());
    let mut default_seen: Option<String> = None;
    for cal in raw.calendars {
        if calendar_ids.insert(cal.id.clone(), ()).is_some() {
            return Err(format!("duplicate calendar id {:?}", cal.id));
        }
        if cal.is_default {
            if let Some(prev) = &default_seen {
                return Err(format!(
                    "fixture has two default calendars: {prev:?} and {:?} - is_default = true must be unique",
                    cal.id
                ));
            }
            default_seen = Some(cal.id.clone());
        }
        calendars.push(Calendar {
            id: cal.id,
            name: cal.name,
            color: cal.color,
            is_default: cal.is_default,
        });
    }
    let mut event_ids: HashMap<String, ()> = HashMap::new();
    let mut events = Vec::with_capacity(raw.events.len());
    for ev in raw.events {
        if event_ids.insert(ev.id.clone(), ()).is_some() {
            return Err(format!("duplicate event id {:?}", ev.id));
        }
        if !calendar_ids.contains_key(&ev.calendar_id) {
            return Err(format!(
                "event {:?} references unknown calendar {:?}",
                ev.id, ev.calendar_id
            ));
        }
        let start =
            parse_ts(&ev.start).map_err(|e| format!("event {:?} start: {e}", ev.id))?;
        let end = parse_ts(&ev.end).map_err(|e| format!("event {:?} end: {e}", ev.id))?;
        events.push(Event {
            id: ev.id,
            calendar_id: ev.calendar_id,
            subject: ev.subject,
            body_preview: ev.body_preview,
            body_text: ev.body_text,
            start,
            end,
            location: ev.location,
            organizer: ev.organizer.map(Address::from),
            attendees: ev.attendees.into_iter().map(Address::from).collect(),
            is_all_day: ev.is_all_day,
        });
    }

    // Contact folders + contacts. Same shape as calendars + events:
    // every contact references a declared folder, ids are unique
    // within their kind.
    let mut contact_folder_ids: HashMap<String, ()> = HashMap::new();
    let mut contact_folders = Vec::with_capacity(raw.contact_folders.len());
    let mut default_contact_folder_seen: Option<String> = None;
    for cf in raw.contact_folders {
        if contact_folder_ids.insert(cf.id.clone(), ()).is_some() {
            return Err(format!("duplicate contact_folder id {:?}", cf.id));
        }
        if cf.is_default {
            if let Some(prev) = &default_contact_folder_seen {
                return Err(format!(
                    "fixture has two default contact folders: {prev:?} and {:?} - is_default = true must be unique",
                    cf.id
                ));
            }
            default_contact_folder_seen = Some(cf.id.clone());
        }
        if let Some(parent) = &cf.parent_folder_id
            && !contact_folder_ids.contains_key(parent)
        {
            // Forward reference: same rule as mailbox parent_id, the
            // parent must already have been declared.
            return Err(format!(
                "contact_folder {:?}: parent_folder_id {parent:?} does not exist",
                cf.id
            ));
        }
        contact_folders.push(ContactFolder {
            id: cf.id,
            display_name: cf.display_name,
            parent_folder_id: cf.parent_folder_id,
            is_default: cf.is_default,
        });
    }
    let mut contact_ids: HashMap<String, ()> = HashMap::new();
    let mut contacts = Vec::with_capacity(raw.contacts.len());
    for c in raw.contacts {
        if contact_ids.insert(c.id.clone(), ()).is_some() {
            return Err(format!("duplicate contact id {:?}", c.id));
        }
        if !contact_folder_ids.contains_key(&c.folder_id) {
            return Err(format!(
                "contact {:?} references unknown folder {:?}",
                c.id, c.folder_id
            ));
        }
        contacts.push(Contact {
            id: c.id,
            folder_id: c.folder_id,
            display_name: c.display_name,
            emails: c.emails.into_iter().map(ContactEmail::from).collect(),
        });
    }

    // Change-script projection. Normalised against the *baseline*
    // mailbox set declared in the same fixture; mailboxes that a
    // later step's `mailbox_create` op introduces are not visible
    // here (matching the Lua loader's snapshot semantics, see
    // `src/lua.rs::read_email_create`).
    let mut change_script: Vec<ChangeStep> = Vec::with_capacity(raw.change_script.len());
    for raw_step in raw.change_script {
        change_script.push(normalize_change_step(raw_step, &mb_ids, fixture_dir)?);
    }

    // Load-time IMAP UID assignment: each declared email gets its
    // mailbox UIDs in fixture declaration order. The Nth email
    // membership in a given mailbox lands at uid N + 1, matching
    // what the pre-fix filter-then-enumerate path produced for an
    // unmutated fixture (so existing fixtures, tests, and on-the-
    // wire UID values are unchanged at load).
    let mut mailbox_uid_history: std::collections::HashMap<String, Vec<Option<String>>> =
        std::collections::HashMap::new();
    for email in &emails {
        for mb in &email.mailbox_ids {
            mailbox_uid_history
                .entry(mb.clone())
                .or_default()
                .push(Some(email.id.clone()));
        }
    }

    let state = raw.state.unwrap_or_else(|| "fixture-state".to_string());
    let change_log = ChangeLog::seed(&state);
    // Seed the counter to the larger of (a) the highest declared
    // `mock-X-N` suffix and (b) the loaded resource count. (a) is
    // the strict collision-avoidance guarantee. (b) preserves the
    // pre-fix wire shape: a fixture with two `email-001` /
    // `email-002` declared emails (no `mock-email-` ids) used to
    // mint `mock-email-3` because the old code did `len() + 1`.
    // Tests pin that value, so the counter starts at len when no
    // mock-prefixed declarations beat it.
    let synthetic_event_seq = max_mock_seq(events.iter().map(|e| e.id.as_str()), "mock-event-")
        .max(events.len() as u64);
    let synthetic_email_seq = max_mock_seq(emails.iter().map(|e| e.id.as_str()), "mock-email-")
        .max(emails.len() as u64);
    Ok(Fixture {
        name: raw.name,
        state,
        account: Account {
            id: raw.account.id,
            name: raw.account.name,
        },
        mailboxes,
        emails,
        oauth,
        calendars,
        events,
        contact_folders,
        contacts,
        change_log,
        change_script,
        mailbox_uid_history,
        synthetic_event_seq,
        synthetic_email_seq,
    })
}

/// Highest `<prefix>N` suffix value seen across `ids`. Used at
/// load time to seed `synthetic_event_seq` / `synthetic_email_seq`
/// so the first synthesized id is strictly greater than any
/// pre-declared one. Ids that don't match the prefix or have a
/// non-numeric suffix are ignored.
fn max_mock_seq<'a, I: Iterator<Item = &'a str>>(ids: I, prefix: &str) -> u64 {
    ids.filter_map(|id| id.strip_prefix(prefix))
        .filter_map(|n| n.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Build an `EmailUpdate` op from the (id, keywords?, mailbox_ids?)
/// tuple both authoring paths produce. Centralises the JMAP patch
/// shape (`keywords` / `mailboxIds` as `{flag-or-id: true}` maps);
/// callers that need a per-entry error context wrap the returned
/// error with their own framing.
pub(crate) fn email_update_op(
    id: String,
    keywords: Option<Vec<String>>,
    mailbox_ids: Option<Vec<String>>,
) -> Result<ChangeOp, &'static str> {
    let mut patch = serde_json::Map::new();
    if let Some(kw) = keywords {
        patch.insert("keywords".into(), bool_map(kw));
    }
    if let Some(mids) = mailbox_ids {
        patch.insert("mailboxIds".into(), bool_map(mids));
    }
    if patch.is_empty() {
        return Err("at least one of keywords / mailbox_ids must be set");
    }
    Ok(ChangeOp::EmailUpdate {
        id,
        patch: serde_json::Value::Object(patch),
    })
}

/// Build an `EmailMove` op. Empty `mailbox_ids` is rejected (a move
/// to nowhere would orphan the email).
pub(crate) fn email_move_op(
    id: String,
    mailbox_ids: Vec<String>,
) -> Result<ChangeOp, &'static str> {
    if mailbox_ids.is_empty() {
        return Err("mailbox_ids must be non-empty");
    }
    Ok(ChangeOp::EmailMove { id, mailbox_ids })
}

/// Build a `MailboxCreate` op. Role string is parsed into the
/// canonical [`Role`] enum if present.
pub(crate) fn mailbox_create_op(raw: RawMailbox) -> Result<ChangeOp, String> {
    let role = raw.role.as_deref().map(Role::parse).transpose()?;
    Ok(ChangeOp::MailboxCreate(Box::new(Mailbox {
        id: raw.id,
        name: raw.name,
        role,
        parent_id: raw.parent_id,
        sort_order: raw.sort_order,
        is_subscribed: raw.is_subscribed.unwrap_or(true),
    })))
}

/// Build a `MailboxUpdate` op from the JMAP-shape patch fields.
/// Field names follow `Mailbox/set` wire convention (camelCase) so
/// the produced patch routes through the same `apply_mailbox_patch`
/// the JMAP `Mailbox/set` mutator uses.
pub(crate) fn mailbox_update_op(
    id: String,
    name: Option<String>,
    parent_id: Option<String>,
    sort_order: Option<i64>,
    role: Option<String>,
    is_subscribed: Option<bool>,
) -> Result<ChangeOp, &'static str> {
    let mut patch = serde_json::Map::new();
    if let Some(name) = name {
        patch.insert("name".into(), serde_json::Value::String(name));
    }
    if let Some(p) = parent_id {
        patch.insert("parentId".into(), serde_json::Value::String(p));
    }
    if let Some(s) = sort_order {
        patch.insert(
            "sortOrder".into(),
            serde_json::Value::Number(serde_json::Number::from(s)),
        );
    }
    if let Some(r) = role {
        patch.insert("role".into(), serde_json::Value::String(r));
    }
    if let Some(b) = is_subscribed {
        patch.insert("isSubscribed".into(), serde_json::Value::Bool(b));
    }
    if patch.is_empty() {
        return Err("at least one field must be set");
    }
    Ok(ChangeOp::MailboxUpdate {
        id,
        patch: serde_json::Value::Object(patch),
    })
}

/// Build an `EventCreate` op. Parses RFC 3339 start / end timestamps;
/// the resulting `Event` carries the live fixture's organizer /
/// attendee / location semantics.
pub(crate) fn event_create_op(raw: RawEvent) -> Result<ChangeOp, String> {
    let id_for_msg = raw.id.clone();
    let start = parse_ts(&raw.start).map_err(|e| format!("{id_for_msg:?} start: {e}"))?;
    let end = parse_ts(&raw.end).map_err(|e| format!("{id_for_msg:?} end: {e}"))?;
    Ok(ChangeOp::EventCreate(Box::new(Event {
        id: raw.id,
        calendar_id: raw.calendar_id,
        subject: raw.subject,
        body_preview: raw.body_preview,
        body_text: raw.body_text,
        start,
        end,
        location: raw.location,
        organizer: raw.organizer.map(Address::from),
        attendees: raw.attendees.into_iter().map(Address::from).collect(),
        is_all_day: raw.is_all_day,
    })))
}

/// Build an `EventUpdate` op. Field names follow the change-script
/// projection (snake_case `body_text`, plain RFC 3339 `start` / `end`
/// strings rather than the Graph nested `start.dateTime` form).
pub(crate) fn event_update_op(
    id: String,
    subject: Option<String>,
    start: Option<String>,
    end: Option<String>,
    location: Option<String>,
    body_text: Option<String>,
) -> Result<ChangeOp, &'static str> {
    let mut patch = serde_json::Map::new();
    if let Some(s) = subject {
        patch.insert("subject".into(), serde_json::Value::String(s));
    }
    if let Some(s) = start {
        patch.insert("start".into(), serde_json::Value::String(s));
    }
    if let Some(s) = end {
        patch.insert("end".into(), serde_json::Value::String(s));
    }
    if let Some(s) = location {
        patch.insert("location".into(), serde_json::Value::String(s));
    }
    if let Some(s) = body_text {
        patch.insert("body_text".into(), serde_json::Value::String(s));
    }
    if patch.is_empty() {
        return Err("at least one field must be set");
    }
    Ok(ChangeOp::EventUpdate {
        id,
        patch: serde_json::Value::Object(patch),
    })
}

/// Build a `ContactFolderCreate` op.
pub(crate) fn contact_folder_create_op(raw: RawContactFolder) -> ChangeOp {
    ChangeOp::ContactFolderCreate(Box::new(ContactFolder {
        id: raw.id,
        display_name: raw.display_name,
        parent_folder_id: raw.parent_folder_id,
        is_default: raw.is_default,
    }))
}

/// Build a `ContactFolderUpdate` op. Field names are snake_case
/// (no JMAP wire equivalent).
pub(crate) fn contact_folder_update_op(
    id: String,
    display_name: Option<String>,
    parent_folder_id: Option<String>,
) -> Result<ChangeOp, &'static str> {
    let mut patch = serde_json::Map::new();
    if let Some(n) = display_name {
        patch.insert("display_name".into(), serde_json::Value::String(n));
    }
    if let Some(p) = parent_folder_id {
        patch.insert(
            "parent_folder_id".into(),
            serde_json::Value::String(p),
        );
    }
    if patch.is_empty() {
        return Err("at least one field must be set");
    }
    Ok(ChangeOp::ContactFolderUpdate {
        id,
        patch: serde_json::Value::Object(patch),
    })
}

/// Build a `ContactCreate` op.
pub(crate) fn contact_create_op(raw: RawContact) -> ChangeOp {
    ChangeOp::ContactCreate(Box::new(Contact {
        id: raw.id,
        folder_id: raw.folder_id,
        display_name: raw.display_name,
        emails: raw.emails.into_iter().map(ContactEmail::from).collect(),
    }))
}

/// Build a `ContactUpdate` op. The `emails` field, when provided,
/// is a full-replace projection.
pub(crate) fn contact_update_op(
    id: String,
    display_name: Option<String>,
    folder_id: Option<String>,
    emails: Option<Vec<ContactEmail>>,
) -> Result<ChangeOp, &'static str> {
    let mut patch = serde_json::Map::new();
    if let Some(n) = display_name {
        patch.insert("display_name".into(), serde_json::Value::String(n));
    }
    if let Some(f) = folder_id {
        patch.insert("folder_id".into(), serde_json::Value::String(f));
    }
    if let Some(emails) = emails {
        let arr: Vec<serde_json::Value> = emails
            .into_iter()
            .map(|e| {
                let ContactEmail { address, name } = e;
                let mut obj = serde_json::Map::new();
                obj.insert("address".into(), serde_json::Value::String(address));
                if let Some(n) = name {
                    obj.insert("name".into(), serde_json::Value::String(n));
                }
                serde_json::Value::Object(obj)
            })
            .collect();
        patch.insert("emails".into(), serde_json::Value::Array(arr));
    }
    if patch.is_empty() {
        return Err("at least one field must be set");
    }
    Ok(ChangeOp::ContactUpdate {
        id,
        patch: serde_json::Value::Object(patch),
    })
}

/// JMAP-shape `{key: true}` map for a `Vec<String>` of keywords or
/// mailbox ids. Both `Email/set` (apply layer) and the change-script
/// patch builders consume this shape.
fn bool_map(keys: Vec<String>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for k in keys {
        m.insert(k, serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(m)
}

// ── Patch appliers (change-script side) ─────────────────────────────
//
// These operate on canonical fixture types (`Contact`,
// `ContactFolder`, `Event`) using the snake_case patch shape the
// change-script projection emits. The JMAP wire-shape patches
// (`apply_email_patch`, `apply_mailbox_patch`) live in `src/jmap.rs`
// next to `Email/set` / `Mailbox/set`; the Graph wire-shape event
// patch (`graph::calendar::apply_event_patch`) decodes Graph's
// nested `start.dateTime` / `end.dateTime` form. The change-script
// path keeps its own helper here because flat RFC3339 strings are
// the projection ratatoskr's harness drives via TOML / Lua.

/// Apply a snake_case patch (`display_name`, `parent_folder_id`)
/// to a [`ContactFolder`]. Used by change-script
/// `contact_folder_update` ops.
pub(crate) fn apply_contact_folder_patch(
    folder: &mut ContactFolder,
    patch: &serde_json::Value,
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "display_name" => {
                folder.display_name = v
                    .as_str()
                    .ok_or_else(|| "display_name must be a string".to_string())?
                    .to_string();
            }
            "parent_folder_id" => {
                folder.parent_folder_id = match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => return Err("parent_folder_id must be a string or null".to_string()),
                };
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

/// Apply a snake_case patch (`display_name`, `folder_id`, `emails`)
/// to a [`Contact`]. `folder_id` updates that change the value are
/// rejected: cross-folder moves can't be expressed as a single
/// update because the source-folder `contacts/delta` walk filters
/// by current `folder_id` and would never see the moved contact.
/// Real Microsoft Graph doesn't expose `folder_id` as a writable
/// property either; clients destroy + create.
pub(crate) fn apply_contact_patch(
    contact: &mut Contact,
    patch: &serde_json::Value,
    folders: &[ContactFolder],
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "display_name" => {
                contact.display_name = match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => return Err("display_name must be a string or null".to_string()),
                };
            }
            "folder_id" => {
                let id = v
                    .as_str()
                    .ok_or_else(|| "folder_id must be a string".to_string())?;
                if !folders.iter().any(|f| f.id == id) {
                    return Err(format!("folder_id {id:?} not in fixture"));
                }
                if id != contact.folder_id {
                    return Err(format!(
                        "folder_id update from {old:?} to {new:?} not supported - issue contact_destroy + contact_create instead",
                        old = contact.folder_id,
                        new = id,
                    ));
                }
                contact.folder_id = id.to_string();
            }
            "emails" => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| "emails must be an array".to_string())?;
                let mut out = Vec::with_capacity(arr.len());
                for e in arr {
                    let address = e
                        .get("address")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "emails entry missing address".to_string())?
                        .to_string();
                    let name = e
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    out.push(ContactEmail { address, name });
                }
                contact.emails = out;
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

/// Apply a flat-RFC3339 patch (`subject`, `start`, `end`,
/// `location`, `body_text`) to an [`Event`]. Mirrors the keys the
/// change-script `event_update` builder emits. Distinct from
/// `graph::calendar::apply_event_patch` which decodes Graph's
/// nested `start.dateTime` shape.
pub(crate) fn apply_change_event_patch(
    event: &mut Event,
    patch: &serde_json::Value,
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "subject" => {
                event.subject = v
                    .as_str()
                    .ok_or_else(|| "subject must be a string".to_string())?
                    .to_string();
            }
            "start" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "start must be an RFC3339 string".to_string())?;
                event.start = parse_ts(s)?;
            }
            "end" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "end must be an RFC3339 string".to_string())?;
                event.end = parse_ts(s)?;
            }
            "location" => {
                event.location = match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => return Err("location must be a string or null".to_string()),
                };
            }
            "body_text" => {
                event.body_text = match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => return Err("body_text must be a string or null".to_string()),
                };
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

/// Project one [`RawChangeStep`] into a [`ChangeStep`]. Op order in
/// the produced `Vec<ChangeOp>` matches the Lua change builder
/// (`src/lua.rs::builder_change`); patches are constructed via the
/// per-op helper functions above so a TOML and Lua change step
/// that name the same fields produce byte-identical `ChangeStep`s.
/// Used by both the TOML loader and (transitively) by the Lua
/// loader; the email_create path here additionally validates
/// against the fixture's baseline mailbox set, matching what the
/// Lua side does.
fn normalize_change_step(
    raw: RawChangeStep,
    mb_ids: &HashMap<String, ()>,
    fixture_dir: &Path,
) -> Result<ChangeStep, String> {
    let id = raw.id;
    let mut ops: Vec<ChangeOp> = Vec::new();

    for em in raw.email_create {
        if !em.attachments.is_empty() {
            return Err(format!(
                "change step {id:?}: email_create entry {:?}: attachments are not supported in change scripts (v0)",
                em.id
            ));
        }
        let email_id = em.id.clone();
        let email = normalize_email(em, mb_ids, fixture_dir)
            .map_err(|e| format!("change step {id:?}: email_create entry {email_id:?}: {e}"))?;
        ops.push(ChangeOp::EmailCreate(Box::new(email)));
    }
    for u in raw.email_update {
        let entry_id = u.id.clone();
        ops.push(email_update_op(u.id, u.keywords, u.mailbox_ids).map_err(|e| {
            format!("change step {id:?}: email_update entry {entry_id:?}: {e}")
        })?);
    }
    for m in raw.email_move {
        let entry_id = m.id.clone();
        ops.push(email_move_op(m.id, m.mailbox_ids).map_err(|e| {
            format!("change step {id:?}: email_move entry {entry_id:?}: {e}")
        })?);
    }
    for d in raw.email_destroy {
        ops.push(ChangeOp::EmailDestroy { id: d });
    }

    for mb in raw.mailbox_create {
        let entry_id = mb.id.clone();
        ops.push(mailbox_create_op(mb).map_err(|e| {
            format!("change step {id:?}: mailbox_create {entry_id:?}: {e}")
        })?);
    }
    for u in raw.mailbox_update {
        let entry_id = u.id.clone();
        ops.push(
            mailbox_update_op(
                u.id,
                u.name,
                u.parent_id,
                u.sort_order,
                u.role,
                u.is_subscribed,
            )
            .map_err(|e| {
                format!("change step {id:?}: mailbox_update entry {entry_id:?}: {e}")
            })?,
        );
    }
    for d in raw.mailbox_destroy {
        ops.push(ChangeOp::MailboxDestroy { id: d });
    }

    for ev in raw.event_create {
        ops.push(
            event_create_op(ev)
                .map_err(|e| format!("change step {id:?}: event_create {e}"))?,
        );
    }
    for u in raw.event_update {
        let entry_id = u.id.clone();
        ops.push(
            event_update_op(u.id, u.subject, u.start, u.end, u.location, u.body_text)
                .map_err(|e| {
                    format!("change step {id:?}: event_update entry {entry_id:?}: {e}")
                })?,
        );
    }
    for d in raw.event_destroy {
        ops.push(ChangeOp::EventDestroy { id: d });
    }

    for cf in raw.contact_folder_create {
        ops.push(contact_folder_create_op(cf));
    }
    for u in raw.contact_folder_update {
        let entry_id = u.id.clone();
        ops.push(
            contact_folder_update_op(u.id, u.display_name, u.parent_folder_id)
                .map_err(|e| {
                    format!(
                        "change step {id:?}: contact_folder_update entry {entry_id:?}: {e}"
                    )
                })?,
        );
    }
    for d in raw.contact_folder_destroy {
        ops.push(ChangeOp::ContactFolderDestroy { id: d });
    }

    for c in raw.contact_create {
        ops.push(contact_create_op(c));
    }
    for u in raw.contact_update {
        let entry_id = u.id.clone();
        let emails = u
            .emails
            .map(|v| v.into_iter().map(ContactEmail::from).collect());
        ops.push(
            contact_update_op(u.id, u.display_name, u.folder_id, emails).map_err(|e| {
                format!("change step {id:?}: contact_update entry {entry_id:?}: {e}")
            })?,
        );
    }
    for d in raw.contact_destroy {
        ops.push(ChangeOp::ContactDestroy { id: d });
    }

    Ok(ChangeStep { id, ops })
}

pub fn parse_ts(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid RFC3339 timestamp {s:?}: {e}"))
}

/// Normalise one [`RawEmail`] into a typed [`Email`]: timestamp parsing,
/// body extraction, attachment loading, and cross-reference checks
/// against `mb_ids` (the set of mailbox ids known to the surrounding
/// fixture). Factored out so the change-script apply path can reuse the
/// same validation when an `email_create` op fires at runtime.
///
/// `mb_ids` is the surrounding mailbox-id set keyed for `O(1)` lookup
/// the same way the load-time validator builds it; the change-script
/// path passes a freshly-built set from the live fixture's mailboxes.
pub(crate) fn normalize_email(
    em: RawEmail,
    mb_ids: &HashMap<String, ()>,
    fixture_dir: &Path,
) -> Result<Email, String> {
    if em.mailbox_ids.is_empty() {
        return Err(format!("email {:?}: mailbox_ids must not be empty", em.id));
    }
    for mid in &em.mailbox_ids {
        if !mb_ids.contains_key(mid) {
            return Err(format!(
                "email {:?}: mailbox_ids contains unknown {mid:?}",
                em.id
            ));
        }
    }

    let body = match em.body_text {
        Some(t) => Body::Text(t),
        None => {
            return Err(format!(
                "email {:?}: must declare body_text (body_path/body_html not yet implemented)",
                em.id
            ));
        }
    };

    let received_at =
        parse_ts(&em.received_at).map_err(|e| format!("email {:?} received_at: {e}", em.id))?;
    let sent_at = match em.sent_at {
        Some(s) => parse_ts(&s).map_err(|e| format!("email {:?} sent_at: {e}", em.id))?,
        None => received_at,
    };

    let size = em.size.unwrap_or_else(|| match &body {
        Body::Text(s) => i64::try_from(s.len()).unwrap_or(i64::MAX),
    });

    let mut attachments = Vec::with_capacity(em.attachments.len());
    let mut blob_ids: HashMap<String, ()> = HashMap::new();
    for raw_att in em.attachments {
        if blob_ids.insert(raw_att.blob_id.clone(), ()).is_some() {
            return Err(format!(
                "email {:?}: duplicate attachment blob_id {:?}",
                em.id, raw_att.blob_id
            ));
        }
        let disposition = match raw_att.disposition.as_deref() {
            Some(s) => Disposition::parse(s)
                .map_err(|e| format!("email {:?} attachment {:?}: {e}", em.id, raw_att.blob_id))?,
            None => Disposition::Attachment,
        };
        let blob_path = fixture_dir.join(&raw_att.data_path);
        let data = std::fs::read(&blob_path).map_err(|e| {
            format!(
                "email {:?} attachment {:?}: read {}: {e}",
                em.id,
                raw_att.blob_id,
                blob_path.display()
            )
        })?;
        let size = raw_att
            .size
            .unwrap_or_else(|| i64::try_from(data.len()).unwrap_or(i64::MAX));
        attachments.push(Attachment {
            blob_id: raw_att.blob_id,
            name: raw_att.name,
            content_type: raw_att.content_type,
            size,
            disposition,
            cid: raw_att.cid,
            data,
        });
    }

    let has_attachment = match em.has_attachment {
        Some(false) if !attachments.is_empty() => {
            return Err(format!(
                "email {:?}: has_attachment=false but {} attachment(s) declared",
                em.id,
                attachments.len()
            ));
        }
        Some(b) => b,
        None => !attachments.is_empty(),
    };

    if em.body_raw_bytes.is_some() && !attachments.is_empty() {
        return Err(format!(
            "email {:?}: body_raw_bytes is mutually exclusive with attachments (the raw block is the entire body)",
            em.id
        ));
    }

    Ok(Email {
        thread_id: em.thread_id.unwrap_or_else(|| em.id.clone()),
        id: em.id,
        mailbox_ids: em.mailbox_ids,
        keywords: em.keywords,
        size,
        received_at,
        sent_at,
        from: em.from.map(Address::from),
        to: em.to.into_iter().map(Address::from).collect(),
        cc: em.cc.into_iter().map(Address::from).collect(),
        bcc: em.bcc.into_iter().map(Address::from).collect(),
        reply_to: em.reply_to.into_iter().map(Address::from).collect(),
        subject: em.subject,
        preview: em.preview,
        message_id: em.message_id,
        in_reply_to: em.in_reply_to,
        references: em.references,
        has_attachment,
        body,
        attachments,
        raw_bytes: em.body_raw_bytes,
    })
}

/// Cheap email-shape check for `account.name`. Not RFC 5322
/// compliant - we just want to catch the obvious "Display Name"
/// mistake before a downstream OAuth client trips on it.
/// Requires exactly one `@`, non-empty local and domain parts,
/// and no whitespace anywhere. Domains without a dot are allowed
/// (e.g. `user@localhost`) since fixtures sometimes use short
/// hosts in tests.
fn is_email_shaped(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty()
}

/// Walk parent_id chains starting from each mailbox; report any cycle.
///
/// Each parent_id is already validated to exist by the caller, so the
/// `expect()` below is sound.
fn detect_cycles(mailboxes: &[Mailbox]) -> Result<(), String> {
    let by_id: HashMap<&str, &Mailbox> =
        mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();
    for start in mailboxes {
        let mut cur = start;
        for _ in 0..mailboxes.len() {
            let Some(parent_id) = &cur.parent_id else {
                break;
            };
            cur = by_id
                .get(parent_id.as_str())
                .copied()
                .expect("parent_id existence validated upstream");
            if cur.id == start.id {
                return Err(format!("mailbox parent cycle including {:?}", start.id));
            }
        }
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        name = "test"

        [account]
        id = "a1"
        name = "alice@example.com"
        is_personal = true

        [[mailbox]]
        id = "mb-inbox"
        name = "Inbox"
        role = "inbox"

        [[email]]
        id = "e1"
        mailbox_ids = ["mb-inbox"]
        received_at = "2026-01-15T10:00:00Z"
        from = "alice@example.com"
        to = ["bob@example.com"]
        subject = "hi"
        body_text = "hello"
    "#;

    fn parse(s: &str) -> Result<Fixture, String> {
        let raw: RawFixture = toml::from_str(s).map_err(|e| e.to_string())?;
        normalize(raw)
    }

    #[test]
    fn loads_minimal_fixture() {
        let fix = parse(MINIMAL).unwrap();
        assert_eq!(fix.name, "test");
        assert_eq!(fix.account.id, "a1");
        assert_eq!(fix.mailboxes.len(), 1);
        assert_eq!(fix.mailboxes[0].role, Some(Role::Inbox));
        assert!(fix.mailboxes[0].is_subscribed);
        assert_eq!(fix.emails.len(), 1);
        assert_eq!(fix.emails[0].thread_id, "e1");
        assert_eq!(fix.emails[0].mailbox_ids, vec!["mb-inbox".to_string()]);
        assert_eq!(fix.emails[0].sent_at, fix.emails[0].received_at);
        assert!(matches!(&fix.emails[0].body, Body::Text(t) if t == "hello"));
        assert_eq!(fix.emails[0].size, 5);
        assert_eq!(fix.emails[0].from.as_ref().unwrap().email, "alice@example.com");
        assert!(fix.emails[0].from.as_ref().unwrap().name.is_none());
    }

    #[test]
    fn defaults_state_token() {
        let fix = parse(MINIMAL).unwrap();
        assert_eq!(fix.state, "fixture-state");
    }

    #[test]
    fn rejects_non_personal_account() {
        let s = MINIMAL.replace("is_personal = true", "is_personal = false");
        let err = parse(&s).unwrap_err();
        assert!(err.contains("is_personal"), "got: {err}");
    }

    #[test]
    fn rejects_account_name_without_email_shape() {
        let s = MINIMAL.replace(r#"name = "alice@example.com""#, r#"name = "Alice""#);
        let err = parse(&s).unwrap_err();
        assert!(err.contains("email-shaped"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_mailbox_ref() {
        let s = MINIMAL.replace(r#"["mb-inbox"]"#, r#"["nope"]"#);
        let err = parse(&s).unwrap_err();
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn rejects_duplicate_mailbox_id() {
        let s = format!(
            r#"{MINIMAL}
            [[mailbox]]
            id = "mb-inbox"
            name = "Inbox-Dup"
            "#
        );
        let err = parse(&s).unwrap_err();
        assert!(err.contains("duplicate mailbox id"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_role() {
        let s = MINIMAL.replace(r#"role = "inbox""#, r#"role = "weird""#);
        let err = parse(&s).unwrap_err();
        assert!(err.contains("unknown role"), "got: {err}");
    }

    #[test]
    fn detects_parent_cycle() {
        let s = r#"
            name = "x"
            [account]
            id = "a"
            name = "a@b"
            is_personal = true
            [[mailbox]]
            id = "m1"
            name = "M1"
            parent_id = "m2"
            [[mailbox]]
            id = "m2"
            name = "M2"
            parent_id = "m1"
        "#;
        let err = parse(s).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn full_address_table_form() {
        let s = MINIMAL.replace(
            r#"from = "alice@example.com""#,
            r#"from = { name = "Alice", email = "alice@example.com" }"#,
        );
        let fix = parse(&s).unwrap();
        let from = fix.emails[0].from.as_ref().unwrap();
        assert_eq!(from.name.as_deref(), Some("Alice"));
        assert_eq!(from.email, "alice@example.com");
    }
}
