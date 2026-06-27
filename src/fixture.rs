//! Fixture loader and validator.
//!
//! Reads a TOML fixture, normalises raw values into typed structs, and
//! enforces the invariants documented in `notes/fixture-format.md`. The
//! returned [`Fixture`] is read-only and feeds every JMAP response.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    /// Shared state-token seed (e.g. `"fixture-state"`). Every
    /// account's state string is `{seed}` at load and `{seed}.{N}`
    /// after its Nth mutation; the seed is the same value for every
    /// account so the primary's wire tokens stay byte-identical to
    /// the pre-per-account-state format (~40 tests pin
    /// `"fixture-state.N"`). Per-account independence comes from each
    /// account owning its own counter inside [`Self::account_logs`],
    /// not from a per-account seed string. See
    /// `notes/per-account-state.md`.
    pub state_seed: String,
    /// Declared accounts. v0 ships at least one and at most one is
    /// flagged `primary` (the v0 mail account every protocol surface
    /// scopes to today). Multi-account fixtures are accepted by the
    /// loader but until Stage 2 of the multi-account refactor lands,
    /// every resource (mailbox, email, calendar, contact, category)
    /// is implicitly scoped to the primary account: declaring an
    /// extra account today simply means the OAuth provider knows
    /// about it and the JMAP session resource could enumerate it
    /// later, but no listener serves resources for it yet. The
    /// follow-up that lands Graph groups / shared mailbox sync grows
    /// per-resource `account_id` and per-protocol scoping.
    pub accounts: Vec<Account>,
    pub mailboxes: Vec<Mailbox>,
    pub emails: Vec<Email>,
    pub oauth: OAuthConfig,
    /// Discovery surface (WebFinger / OIDC discovery / Mozilla
    /// autoconfig) served on the JMAP HTTP listener. Empty by
    /// default; fixtures that don't exercise ratatoskr's
    /// account-setup cascade can omit the section. See
    /// `DiscoveryConfig`.
    pub discovery: DiscoveryConfig,
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
    /// Outlook master category list. Projects over the Graph
    /// `/v1.0/me/outlook/masterCategories` surface (the Graph
    /// analogue of Gmail labels / JMAP keywords). Flat per-account
    /// in real Graph - no folder scope. Empty by default.
    pub categories: Vec<Category>,
    /// Microsoft Graph groups. Cross-account by nature: each group
    /// declares a `members` list naming declared `[[account]]` ids
    /// who belong to it. Projects over the Graph `/v1.0/groups/...`
    /// surface plus the `/v1.0/me/memberOf` /
    /// `/v1.0/users/{userId}/memberOf` walkers. Empty by default.
    pub groups: Vec<Group>,
    /// Gmail SendAs identities. Flat per-account: an account may
    /// declare zero or more `[[send_as]]` rows. Real Gmail uses the
    /// `sendAsEmail` as the primary key; v0 enforces uniqueness per
    /// `(account_id, send_as_email)` so a multi-account fixture can
    /// reuse the same address across accounts. Empty by default;
    /// fixtures that don't need sendAs coverage can omit the table.
    pub send_as: Vec<SendAs>,
    /// Per-account mutation logs, keyed by `account_id`. Each log
    /// tracks its own monotonic counter / current state token and its
    /// own bounded ring of transitions, all sharing
    /// [`Self::state_seed`]. A mutation advances only the logs of the
    /// accounts it actually touched, so account B's churn never moves
    /// account A's state token nor evicts A's `sinceState` boundary
    /// (RFC 8620 §1.5.2). An account with no entry here has never been
    /// mutated; its state is the bare seed. Empty at load time. JMAP
    /// `Email/changes` / `Mailbox/changes` and the Graph / gcal /
    /// People / CalDAV delta surfaces walk the requesting account's
    /// log to compute deltas between two known states. See
    /// `notes/per-account-state.md`.
    pub account_logs: BTreeMap<String, ChangeLog>,
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
    /// Monotonic counter for synthesized `mock-category-N` ids
    /// produced by `POST /v1.0/me/outlook/masterCategories` when
    /// the client omits the id. Same shape as
    /// `synthetic_event_seq`.
    pub synthetic_category_seq: u64,
    /// Monotonic counter for synthesized `mock-contact-N` ids
    /// produced by JMAP `ContactCard/set` create. Same shape as
    /// `synthetic_event_seq`.
    pub synthetic_contact_seq: u64,
    /// Volatile blob store backing JMAP `Blob/upload`. Keyed by the
    /// minted `blobId` (`uploadblob-N`), each entry holds the uploaded
    /// content type and bytes. JMAP `Email/import` reads from here to
    /// project the uploaded RFC 822 message into a fixture `Email`,
    /// so bifrost's raw-send path (upload -> import-into-Drafts ->
    /// `EmailSubmission/set`) round-trips. Empty at load; not part of
    /// the authored fixture, so `RawFixture` / `normalize` never touch
    /// it, and `POST /test/fixture/reset` clears it back to the
    /// post-load baseline (also empty).
    pub uploaded_blobs: BTreeMap<String, UploadedBlob>,
    /// Gmail user-label colors, keyed by the backing mailbox id (the
    /// roleless fixture `Mailbox` a Gmail user label projects from).
    /// The canonical `Mailbox` carries no color slot, but Gmail's
    /// `labels.create` / `labels.patch` accept a `{ textColor,
    /// backgroundColor }` object that `labels.list` (the endpoint
    /// bifrost's `containers_list` reads) must echo back so a created
    /// label's color survives a resync. Stored here rather than on
    /// `Mailbox` to keep the cross-protocol canonical type and its TOML
    /// / Lua loaders untouched. Volatile: empty at load, not part of
    /// `RawFixture` / `normalize`, and restored to empty on
    /// `POST /test/fixture/reset` (the baseline clone carries an empty
    /// map). Same lifecycle as `uploaded_blobs`.
    pub gmail_label_colors: BTreeMap<String, GmailLabelColor>,
}

/// A Gmail user-label color pair. Mirrors the Gmail API `color` object
/// (`{ textColor, backgroundColor }`). Stored in
/// [`Fixture::gmail_label_colors`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailLabelColor {
    pub text_color: String,
    pub background_color: String,
}

/// One blob uploaded via JMAP `Blob/upload`. Held in
/// [`Fixture::uploaded_blobs`] until an `Email/import` consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedBlob {
    pub content_type: String,
    pub data: Vec<u8>,
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
    /// Current state token for this account: the bare `seed` while
    /// `counter == 0`, then `{seed}.{counter}` after the first
    /// mutation. Cached here so `Fixture::state_for` is a plain
    /// borrow rather than a re-format on every reporting site.
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from_state: String,
    pub to_state: String,
    pub email_created: Vec<String>,
    pub email_updated: Vec<String>,
    pub email_destroyed: Vec<String>,
    /// Parallel to `email_destroyed`: the `account_id` each destroyed
    /// email belonged to at retire time. Captured at destroy because
    /// the email is gone from the live fixture by the time the
    /// per-account delta walker reads it. Length must equal
    /// `email_destroyed`. Same role as `event_destroyed_parents`.
    pub email_destroyed_accounts: Vec<String>,
    /// Gmail-specific sidecar to `email_updated`: the per-email label-set
    /// delta (Gmail labels added / removed) for each updated email whose
    /// label set actually moved. The generic change log records only
    /// that an email was updated, not which labels changed; the Gmail
    /// `history.list` `labelsAdded` / `labelsRemoved` projection needs
    /// the precise delta (bifrost applies each label add / remove as a
    /// `ScopeChange`). Captured at mutation time by the change-script
    /// step applier; empty for other producers in v0 (incremental Gmail
    /// sync is change-script-driven). See
    /// `notes/ratatoskr-gmail-surface.md`.
    pub email_label_changes: Vec<EmailLabelChange>,
    pub mailbox_created: Vec<String>,
    pub mailbox_updated: Vec<String>,
    pub mailbox_destroyed: Vec<String>,
    /// Parallel to `mailbox_destroyed`. Captured at destroy time.
    pub mailbox_destroyed_accounts: Vec<String>,
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
    pub calendar_created: Vec<String>,
    pub calendar_updated: Vec<String>,
    pub calendar_destroyed: Vec<String>,
    /// Parallel to `calendar_destroyed`: the `account_id` each
    /// destroyed calendar belonged to.
    pub calendar_destroyed_accounts: Vec<String>,
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
    /// Parallel to `contact_folder_destroyed`: the `account_id` each
    /// destroyed contact folder belonged to. Contact folders are the
    /// one folder-like resource that carries no surviving parent to
    /// resolve an account from at destroy time, so the producer must
    /// capture it here (same role as `email_destroyed_accounts`).
    pub contact_folder_destroyed_accounts: Vec<String>,
    pub category_created: Vec<String>,
    pub category_updated: Vec<String>,
    pub category_destroyed: Vec<String>,
    /// Parallel to `category_destroyed`: the `account_id` each
    /// destroyed category belonged to. Categories are flat per-account
    /// with no parent resource, so the producer captures the account
    /// at destroy time.
    pub category_destroyed_accounts: Vec<String>,
}

/// One updated email's Gmail label-set delta. `id` is the email id;
/// `added` / `removed` are Gmail label ids (the `label_ids_for`
/// projection: `INBOX`, `UNREAD`, `STARRED`, `Label_<...>`, ...).
/// Carried alongside `email_updated` so the Gmail `history.list`
/// projection can emit `labelsAdded` / `labelsRemoved` records;
/// every other protocol's `/changes` walker ignores it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailLabelChange {
    pub id: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
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
    /// Parallel to `email_destroyed`: account_id of each destroyed
    /// email. Producers must push to both vectors at the same time
    /// so the per-account delta walker can filter tombstones.
    pub email_destroyed_accounts: Vec<String>,
    /// Gmail label deltas for updated emails. Sidecar to
    /// `email_updated` (see [`Transition::email_label_changes`]).
    /// Routed per-account by email id in `split_diff_by_account`.
    pub email_label_changes: Vec<EmailLabelChange>,
    pub mailbox_created: Vec<String>,
    pub mailbox_updated: Vec<String>,
    pub mailbox_destroyed: Vec<String>,
    /// Parallel to `mailbox_destroyed`.
    pub mailbox_destroyed_accounts: Vec<String>,
    pub event_created: Vec<String>,
    pub event_updated: Vec<String>,
    pub event_destroyed: Vec<String>,
    pub event_destroyed_parents: Vec<String>,
    pub calendar_created: Vec<String>,
    pub calendar_updated: Vec<String>,
    pub calendar_destroyed: Vec<String>,
    pub calendar_destroyed_accounts: Vec<String>,
    pub contact_created: Vec<String>,
    pub contact_updated: Vec<String>,
    pub contact_destroyed: Vec<String>,
    pub contact_destroyed_parents: Vec<String>,
    pub contact_folder_created: Vec<String>,
    pub contact_folder_updated: Vec<String>,
    pub contact_folder_destroyed: Vec<String>,
    /// Parallel to `contact_folder_destroyed`. Producers must push to
    /// both vectors at the same time so the per-account splitter can
    /// route the tombstone to the right log.
    pub contact_folder_destroyed_accounts: Vec<String>,
    pub category_created: Vec<String>,
    pub category_updated: Vec<String>,
    pub category_destroyed: Vec<String>,
    /// Parallel to `category_destroyed`.
    pub category_destroyed_accounts: Vec<String>,
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
            && self.calendar_created.is_empty()
            && self.calendar_updated.is_empty()
            && self.calendar_destroyed.is_empty()
            && self.contact_created.is_empty()
            && self.contact_updated.is_empty()
            && self.contact_destroyed.is_empty()
            && self.contact_folder_created.is_empty()
            && self.contact_folder_updated.is_empty()
            && self.contact_folder_destroyed.is_empty()
            && self.category_created.is_empty()
            && self.category_updated.is_empty()
            && self.category_destroyed.is_empty()
    }
}

/// Extract the trailing counter from a `{seed}.{counter}` state token.
/// The bare seed (no trailing `.<n>`, counter 0) yields `0`. Splits on
/// the last `.` so a seed string that itself contains dots still
/// resolves correctly.
fn counter_of_state(state: &str) -> u64 {
    state
        .rsplit_once('.')
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .unwrap_or(0)
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
            state: seed_state.to_string(),
        }
    }

    /// Advance this account's state token by one and return the
    /// `(from, to)` pair. The caller pushes the freshly-built
    /// transition (whose `from_state` / `to_state` must equal this
    /// pair) into the ring.
    fn advance(&mut self) -> (String, String) {
        let from = self.state.clone();
        self.counter += 1;
        self.state = format!("{}.{}", self.seed, self.counter);
        (from, self.state.clone())
    }

    /// Push a freshly-recorded transition, evicting the oldest when
    /// the ring is full. The seed is never part of the ring, so a
    /// `sinceState == seed` always resolves to the full retained
    /// delta regardless of eviction.
    fn push(&mut self, trans: Transition) {
        if self.transitions.len() >= ChangeLog::MAX_TRANSITIONS {
            self.transitions.pop_front();
        }
        self.transitions.push_back(trans);
    }

    /// Counter of the most recent retained transition that created or
    /// updated `email_id`, or `0` (the seed baseline) when none is
    /// retained. The wire MODSEQ is this value plus one; see
    /// [`Fixture::email_modseq`]. Transitions are appended in counter
    /// order so the last match carries the highest counter, but the
    /// running `max` keeps the result correct regardless of order.
    fn modseq_for_email(&self, email_id: &str) -> u64 {
        let mut best = 0u64;
        for t in &self.transitions {
            if t.email_created.iter().any(|e| e == email_id)
                || t.email_updated.iter().any(|e| e == email_id)
            {
                best = counter_of_state(&t.to_state).max(best);
            }
        }
        best + 1
    }

    /// Walk this account's log for the unfiltered delta between
    /// `since` and the current state. See [`Fixture::delta_since`]
    /// for the resolution rules (`since == state` -> empty, seed or
    /// any retained `from_state` -> walk, otherwise -> `None`).
    fn delta_since<'a, F>(&'a self, since: &str, project: F) -> Option<DeltaSet>
    where
        F: Fn(&'a Transition) -> (&'a Vec<String>, &'a Vec<String>, &'a Vec<String>),
    {
        if since == self.state {
            return Some(DeltaSet::default());
        }
        let seed_known = since == self.seed;
        let head_known = self.transitions.iter().any(|t| t.from_state == since);
        if !seed_known && !head_known {
            return None;
        }
        let mut started = seed_known;
        let mut created: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let mut destroyed: Vec<String> = Vec::new();
        for t in &self.transitions {
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

    /// Like [`Self::delta_since`], but the destroyed walk is filtered
    /// by a parent id (calendar / folder). For each transition the
    /// `parents` projector returns a `Vec<String>` parallel to the
    /// destroyed list; only destroyed ids whose corresponding parent
    /// matches `parent_id` are accumulated. Created / updated are
    /// unfiltered here - the caller filters those against the live
    /// resource's parent.
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
        let seed_known = since == self.seed;
        let head_known = self.transitions.iter().any(|t| t.from_state == since);
        if !seed_known && !head_known {
            return None;
        }
        let mut started = seed_known;
        let mut created: Vec<String> = Vec::new();
        let mut updated: Vec<String> = Vec::new();
        let mut destroyed: Vec<String> = Vec::new();
        for t in &self.transitions {
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
}

impl Fixture {
    /// Borrowing iterator over one account's retained transitions,
    /// oldest first. CalDAV's per-resource ETag / CTag derivation
    /// walks this in reverse to find the last transition that touched
    /// a given event / calendar; the account is resolved from the
    /// calendar / event's owning account. An account with no log yet
    /// yields an empty iterator (the caller falls back to the seed).
    pub fn change_log_transitions_for(
        &self,
        account_id: &str,
    ) -> impl DoubleEndedIterator<Item = &Transition> {
        self.account_logs
            .get(account_id)
            .into_iter()
            .flat_map(|l| l.transitions.iter())
    }

    /// The change-log seed - the state token every account has after
    /// load, before any mutation. Used as the fallback CalDAV state
    /// value when no transition has touched a resource yet.
    pub fn change_log_seed(&self) -> &str {
        &self.state_seed
    }

    /// Current state token for `account_id`: the bare seed when the
    /// account has never been mutated, else `{seed}.{counter}` from
    /// its log. Every per-protocol reporting site resolves the state
    /// it emits through here so a mutation on one account never moves
    /// a sibling's wire token. See `notes/per-account-state.md`.
    pub fn state_for(&self, account_id: &str) -> &str {
        self.account_logs
            .get(account_id)
            .map(|l| l.state.as_str())
            .unwrap_or(&self.state_seed)
    }

    /// Current state token for the primary account. The few genuinely
    /// session-/process-wide reporting sites (JMAP `sessionState`, the
    /// `/test/snapshot-state` + `/test/fixture/step` admin endpoints)
    /// report this rather than any one resource account's token.
    pub fn primary_state(&self) -> &str {
        self.state_for(&self.primary_account().id)
    }

    /// CONDSTORE `HIGHESTMODSEQ` for an account: the per-account change
    /// counter plus one. The `+ 1` keeps it `>= 1` (bifrost rejects a
    /// MODSEQ of 0, `folder_registry.rs` "FETCH returned MODSEQ 0") and
    /// matches the Gmail `historyId` convention (`counter + 1`), so a
    /// never-mutated account reports `HIGHESTMODSEQ 1`. Every mutation
    /// that advances the account's log advances this value, which is
    /// what lets bifrost's `FETCH (CHANGEDSINCE <modseq>)` see a delta.
    pub fn imap_highestmodseq(&self, account_id: &str) -> u64 {
        self.account_logs
            .get(account_id)
            .map(|l| l.counter)
            .unwrap_or(0)
            + 1
    }

    /// Per-message CONDSTORE `MODSEQ` for an email: the change counter
    /// of the most recent transition that created or updated it, plus
    /// one (same `+ 1` baseline as [`Self::imap_highestmodseq`]). An
    /// email untouched since load resolves to `1`. A change evicted
    /// from the bounded ring also falls back to `1`, which is safe: an
    /// evicted change is necessarily older than any modseq a client
    /// could still be holding, so under-reporting it only means the
    /// already-synced message is correctly skipped by `CHANGEDSINCE`.
    pub fn email_modseq(&self, account_id: &str, email_id: &str) -> u64 {
        self.account_logs
            .get(account_id)
            .map(|l| l.modseq_for_email(email_id))
            .unwrap_or(1)
    }

    /// The single account v0 protocol surfaces serve. Multi-account
    /// fixtures still expose exactly one primary; Stage 2 of the
    /// refactor will let listeners scope to a non-primary on a
    /// per-account basis. Normalize guarantees exactly one primary
    /// exists, so the unwrap is structural.
    pub fn primary_account(&self) -> &Account {
        self.accounts
            .iter()
            .find(|a| a.primary)
            .expect("normalize guarantees one primary account")
    }

    /// Borrow the [`Account`] with `id`. Returns None for unknown
    /// ids so callers can map that to the protocol-appropriate
    /// "account not found" error.
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Mailboxes scoped to one account. The full
    /// `fixture.mailboxes` field stays public for the few sites
    /// that need a cross-account view (loader, snapshot, lookup
    /// by id); per-protocol handlers should go through this
    /// helper so a future per-account route addition reads as a
    /// one-line argument change rather than a rewrite.
    /// Mint a fixture-unique mailbox id (`mock-mailbox-<n>`), probing
    /// for a free suffix so a create after a delete never collides with
    /// a surviving id. The bare `mailboxes.len() + 1` scheme is unsafe:
    /// deleting a low-numbered mailbox while a higher one survives lets
    /// the next create reuse the survivor's id. Every protocol's
    /// container-create path goes through here so the id space stays
    /// collision-free across CRUD interleavings.
    pub fn fresh_mailbox_id(&self) -> String {
        let mut n = self.mailboxes.len() + 1;
        loop {
            let candidate = format!("mock-mailbox-{n}");
            if !self.mailboxes.iter().any(|m| m.id == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    pub fn mailboxes_for<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a Mailbox> + 'a {
        self.mailboxes
            .iter()
            .filter(move |m| m.account_id == account_id)
    }

    /// Emails scoped to one account.
    pub fn emails_for<'a>(&'a self, account_id: &'a str) -> impl Iterator<Item = &'a Email> + 'a {
        self.emails
            .iter()
            .filter(move |e| e.account_id == account_id)
    }

    /// Calendars scoped to one account.
    pub fn calendars_for<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a Calendar> + 'a {
        self.calendars
            .iter()
            .filter(move |c| c.account_id == account_id)
    }

    /// Events scoped to one account.
    pub fn events_for<'a>(&'a self, account_id: &'a str) -> impl Iterator<Item = &'a Event> + 'a {
        self.events
            .iter()
            .filter(move |e| e.account_id == account_id)
    }

    /// Contact folders scoped to one account.
    pub fn contact_folders_for<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a ContactFolder> + 'a {
        self.contact_folders
            .iter()
            .filter(move |c| c.account_id == account_id)
    }

    /// Contacts scoped to one account.
    pub fn contacts_for<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a Contact> + 'a {
        self.contacts
            .iter()
            .filter(move |c| c.account_id == account_id)
    }

    /// Categories scoped to one account.
    pub fn categories_for<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a Category> + 'a {
        self.categories
            .iter()
            .filter(move |c| c.account_id == account_id)
    }

    /// SendAs identities scoped to one account.
    pub fn send_as_for<'a>(&'a self, account_id: &'a str) -> impl Iterator<Item = &'a SendAs> + 'a {
        self.send_as
            .iter()
            .filter(move |s| s.account_id == account_id)
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

    /// Category-side analogue. Drives
    /// `POST /v1.0/me/outlook/masterCategories` when the client
    /// posts a body without an `id` field.
    pub fn mint_category_id(&mut self) -> String {
        self.synthetic_category_seq += 1;
        format!("mock-category-{}", self.synthetic_category_seq)
    }

    /// Contact-side analogue. Drives JMAP `ContactCard/set` create
    /// when the client body omits an id.
    pub fn mint_contact_id(&mut self) -> String {
        self.synthetic_contact_seq += 1;
        format!("mock-contact-{}", self.synthetic_contact_seq)
    }

    /// Store an uploaded blob and return its minted `blobId`. The id
    /// is `uploadblob-N` where N is one past the current store size,
    /// so it is deterministic for a given request sequence and never
    /// collides within a run (the store is insert-only short of
    /// `/test/fixture/reset`). Drives JMAP `Blob/upload`.
    pub fn store_uploaded_blob(&mut self, content_type: String, data: Vec<u8>) -> String {
        let blob_id = format!("uploadblob-{}", self.uploaded_blobs.len() + 1);
        self.uploaded_blobs
            .insert(blob_id.clone(), UploadedBlob { content_type, data });
        blob_id
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
        // The aggregate (cross-account) transition we hand back keeps
        // every id the mutation touched and reports the *primary*
        // account's pre/post state. No caller reads the aggregate's
        // from/to for a per-account `newState` (set handlers resolve
        // that via `state_for`; the admin step path reports
        // `primary_state`), but tying it to the primary keeps it
        // meaningful for a single-account fixture (the common case).
        let agg_from = self.primary_state().to_string();
        if diff.is_empty() {
            return transition_from_diff(agg_from.clone(), agg_from, MutationDiff::default());
        }
        // Partition the diff per owning account (lookups run against
        // the just-mutated fixture, so created/updated resources are
        // live; destroyed ids resolve through the parallel
        // account/parent vectors captured at retire time).
        let agg_diff = diff.clone();
        let per_account = self.split_diff_by_account(diff);
        let seed = self.state_seed.clone();
        for (account_id, sub) in per_account {
            let log = self
                .account_logs
                .entry(account_id)
                .or_insert_with(|| ChangeLog::seed(&seed));
            let (from, to) = log.advance();
            let trans = transition_from_diff(from, to, sub);
            log.push(trans);
        }
        let agg_to = self.primary_state().to_string();
        transition_from_diff(agg_from, agg_to, agg_diff)
    }

    /// The set of `account_id`s a `MutationDiff` touches, resolved with
    /// the same per-resource ownership logic [`Self::record_transition`]
    /// uses to advance each account's log. Lets the test-admin
    /// state-mutation path learn exactly which accounts' state advanced
    /// so it can fire push notifications for those accounts only (a
    /// mutation on account B must not push account A). Call after the
    /// mutation has been applied to the live fixture (created / updated
    /// resources resolve through the live fixture; destroyed resources
    /// through the diff's parallel `*_destroyed_accounts` / `*_parents`
    /// vectors). An empty diff yields an empty set.
    pub fn accounts_touched(&self, diff: &MutationDiff) -> std::collections::BTreeSet<String> {
        if diff.is_empty() {
            return std::collections::BTreeSet::new();
        }
        self.split_diff_by_account(diff.clone())
            .into_keys()
            .collect()
    }

    /// Split a `MutationDiff` into per-account sub-diffs. Each id is
    /// routed to its owning account: created/updated resources resolve
    /// through the live fixture (the closure has already applied the
    /// mutation); destroyed resources resolve through the parallel
    /// account vectors (`*_destroyed_accounts`) or, for events /
    /// contacts whose parent still lives, the parent's account. An id
    /// whose account cannot be resolved falls back to the primary
    /// account (defensive; not reachable in v0 where deletes never
    /// cascade across the parent that carries the account).
    fn split_diff_by_account(&self, diff: MutationDiff) -> BTreeMap<String, MutationDiff> {
        let primary = self.primary_account().id.clone();
        let email_acct: HashMap<&str, &str> = self
            .emails
            .iter()
            .map(|e| (e.id.as_str(), e.account_id.as_str()))
            .collect();
        let mailbox_acct: HashMap<&str, &str> = self
            .mailboxes
            .iter()
            .map(|m| (m.id.as_str(), m.account_id.as_str()))
            .collect();
        let calendar_acct: HashMap<&str, &str> = self
            .calendars
            .iter()
            .map(|c| (c.id.as_str(), c.account_id.as_str()))
            .collect();
        let event_cal: HashMap<&str, &str> = self
            .events
            .iter()
            .map(|e| (e.id.as_str(), e.calendar_id.as_str()))
            .collect();
        let folder_acct: HashMap<&str, &str> = self
            .contact_folders
            .iter()
            .map(|f| (f.id.as_str(), f.account_id.as_str()))
            .collect();
        let contact_folder_of: HashMap<&str, &str> = self
            .contacts
            .iter()
            .map(|c| (c.id.as_str(), c.folder_id.as_str()))
            .collect();
        let category_acct: HashMap<&str, &str> = self
            .categories
            .iter()
            .map(|c| (c.id.as_str(), c.account_id.as_str()))
            .collect();

        let resolve_email = |id: &str| {
            email_acct
                .get(id)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        let resolve_mailbox = |id: &str| {
            mailbox_acct
                .get(id)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        let resolve_calendar = |id: &str| {
            calendar_acct
                .get(id)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        // Calendar id -> account (used both for live calendars and as
        // the destroyed-event parent resolver).
        let cal_to_acct = |cal: &str| {
            calendar_acct
                .get(cal)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        let resolve_event = |id: &str| match event_cal.get(id) {
            Some(cal) => cal_to_acct(cal),
            None => primary.clone(),
        };
        let folder_to_acct = |folder: &str| {
            folder_acct
                .get(folder)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        let resolve_contact = |id: &str| match contact_folder_of.get(id) {
            Some(folder) => folder_to_acct(folder),
            None => primary.clone(),
        };
        let resolve_folder = |id: &str| {
            folder_acct
                .get(id)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };
        let resolve_category = |id: &str| {
            category_acct
                .get(id)
                .copied()
                .unwrap_or(primary.as_str())
                .to_string()
        };

        let mut out: BTreeMap<String, MutationDiff> = BTreeMap::new();

        for id in diff.email_created {
            let a = resolve_email(&id);
            out.entry(a).or_default().email_created.push(id);
        }
        for id in diff.email_updated {
            let a = resolve_email(&id);
            out.entry(a).or_default().email_updated.push(id);
        }
        for change in diff.email_label_changes {
            let a = resolve_email(&change.id);
            out.entry(a).or_default().email_label_changes.push(change);
        }
        for (id, acct) in diff
            .email_destroyed
            .into_iter()
            .zip(diff.email_destroyed_accounts)
        {
            let sub = out.entry(acct.clone()).or_default();
            sub.email_destroyed.push(id);
            sub.email_destroyed_accounts.push(acct);
        }
        for id in diff.mailbox_created {
            let a = resolve_mailbox(&id);
            out.entry(a).or_default().mailbox_created.push(id);
        }
        for id in diff.mailbox_updated {
            let a = resolve_mailbox(&id);
            out.entry(a).or_default().mailbox_updated.push(id);
        }
        for (id, acct) in diff
            .mailbox_destroyed
            .into_iter()
            .zip(diff.mailbox_destroyed_accounts)
        {
            let sub = out.entry(acct.clone()).or_default();
            sub.mailbox_destroyed.push(id);
            sub.mailbox_destroyed_accounts.push(acct);
        }
        for id in diff.event_created {
            let a = resolve_event(&id);
            out.entry(a).or_default().event_created.push(id);
        }
        for id in diff.event_updated {
            let a = resolve_event(&id);
            out.entry(a).or_default().event_updated.push(id);
        }
        for (id, parent) in diff
            .event_destroyed
            .into_iter()
            .zip(diff.event_destroyed_parents)
        {
            let a = cal_to_acct(&parent);
            let sub = out.entry(a).or_default();
            sub.event_destroyed.push(id);
            sub.event_destroyed_parents.push(parent);
        }
        for id in diff.calendar_created {
            let a = resolve_calendar(&id);
            out.entry(a).or_default().calendar_created.push(id);
        }
        for id in diff.calendar_updated {
            let a = resolve_calendar(&id);
            out.entry(a).or_default().calendar_updated.push(id);
        }
        for (id, acct) in diff
            .calendar_destroyed
            .into_iter()
            .zip(diff.calendar_destroyed_accounts)
        {
            let sub = out.entry(acct.clone()).or_default();
            sub.calendar_destroyed.push(id);
            sub.calendar_destroyed_accounts.push(acct);
        }
        for id in diff.contact_created {
            let a = resolve_contact(&id);
            out.entry(a).or_default().contact_created.push(id);
        }
        for id in diff.contact_updated {
            let a = resolve_contact(&id);
            out.entry(a).or_default().contact_updated.push(id);
        }
        for (id, parent) in diff
            .contact_destroyed
            .into_iter()
            .zip(diff.contact_destroyed_parents)
        {
            let a = folder_to_acct(&parent);
            let sub = out.entry(a).or_default();
            sub.contact_destroyed.push(id);
            sub.contact_destroyed_parents.push(parent);
        }
        for id in diff.contact_folder_created {
            let a = resolve_folder(&id);
            out.entry(a).or_default().contact_folder_created.push(id);
        }
        for id in diff.contact_folder_updated {
            let a = resolve_folder(&id);
            out.entry(a).or_default().contact_folder_updated.push(id);
        }
        for (id, acct) in diff
            .contact_folder_destroyed
            .into_iter()
            .zip(diff.contact_folder_destroyed_accounts)
        {
            let sub = out.entry(acct.clone()).or_default();
            sub.contact_folder_destroyed.push(id);
            sub.contact_folder_destroyed_accounts.push(acct);
        }
        for id in diff.category_created {
            let a = resolve_category(&id);
            out.entry(a).or_default().category_created.push(id);
        }
        for id in diff.category_updated {
            let a = resolve_category(&id);
            out.entry(a).or_default().category_updated.push(id);
        }
        for (id, acct) in diff
            .category_destroyed
            .into_iter()
            .zip(diff.category_destroyed_accounts)
        {
            let sub = out.entry(acct.clone()).or_default();
            sub.category_destroyed.push(id);
            sub.category_destroyed_accounts.push(acct);
        }

        out
    }

    /// Walk one account's log for an unfiltered delta. Folds the
    /// "account never mutated" case (no log entry) into a seed-only
    /// resolution: `since == seed` is the empty delta, anything else
    /// is `cannotCalculateChanges`.
    fn walk_account<'a, F>(&'a self, since: &str, account_id: &str, project: F) -> Option<DeltaSet>
    where
        F: Fn(&'a Transition) -> (&'a Vec<String>, &'a Vec<String>, &'a Vec<String>),
    {
        match self.account_logs.get(account_id) {
            Some(log) => log.delta_since(since, project),
            None => self.seed_only_delta(since),
        }
    }

    /// Parent-filtered analogue of [`Self::walk_account`].
    fn walk_account_filtered<'a, F, G>(
        &'a self,
        since: &str,
        account_id: &str,
        project: F,
        parents: G,
        parent_id: &str,
    ) -> Option<DeltaSet>
    where
        F: Fn(&'a Transition) -> (&'a Vec<String>, &'a Vec<String>, &'a Vec<String>),
        G: Fn(&'a Transition) -> Option<&'a Vec<String>>,
    {
        match self.account_logs.get(account_id) {
            Some(log) => log.delta_since_filtered_destroys(since, project, parents, parent_id),
            None => self.seed_only_delta(since),
        }
    }

    /// Resolution for an account with no log yet: the seed is the only
    /// known state.
    fn seed_only_delta(&self, since: &str) -> Option<DeltaSet> {
        if since == self.state_seed {
            Some(DeltaSet::default())
        } else {
            None
        }
    }

    /// Per-account email delta. Walks the requesting account's own
    /// log (created during its first mutation), so created / updated /
    /// destroyed ids already belong to that account - no live-account
    /// retain pass is needed. Drives the JMAP `Email/changes` handler.
    /// A mutation on a multi-account fixture's secondary never touches
    /// the primary's log, so the primary's walk returns the empty
    /// delta and its state token does not move (RFC 8620 §1.5.2).
    /// `since` older than the seed (or evicted from the account's
    /// bounded ring) returns `None` -> `cannotCalculateChanges`.
    pub fn email_delta_since_account(&self, since: &str, account_id: &str) -> Option<DeltaSet> {
        self.walk_account(since, account_id, |t| {
            (&t.email_created, &t.email_updated, &t.email_destroyed)
        })
    }

    /// Per-account mailbox delta. Same shape as
    /// [`Self::email_delta_since_account`].
    pub fn mailbox_delta_since_account(&self, since: &str, account_id: &str) -> Option<DeltaSet> {
        self.walk_account(since, account_id, |t| {
            (&t.mailbox_created, &t.mailbox_updated, &t.mailbox_destroyed)
        })
    }

    /// Per-calendar event delta. Drives the Microsoft Graph
    /// `calendarView/delta` and `events/delta` surfaces: a follow-up
    /// call with a known `$deltatoken` returns only the events that
    /// changed since that token, plus a fresh deltaLink. Tokens older
    /// than the seed (or evicted from the owning account's bounded
    /// ring) return `None`; the Graph layer converts that to a full
    /// re-bootstrap.
    ///
    /// All three sets are scoped to the named calendar:
    /// - Destroyed ids are filtered against `event_destroyed_parents`
    ///   recorded at retire time (the event is gone from the live
    ///   fixture by then).
    /// - Created / updated ids are filtered against the live event's
    ///   current `calendar_id`, since the account's log mixes ids
    ///   across every calendar the account owns.
    pub fn event_delta_since(&self, since: &str, calendar_id: &str) -> Option<DeltaSet> {
        // Resolve the calendar's owning account so we walk the right
        // per-account log. An unknown calendar has no transitions of
        // its own; seed-only resolution applies.
        let account_id = match self.calendars.iter().find(|c| c.id == calendar_id) {
            Some(c) => c.account_id.clone(),
            None => return self.seed_only_delta(since),
        };
        let mut delta = self.walk_account_filtered(
            since,
            &account_id,
            |t| (&t.event_created, &t.event_updated, &t.event_destroyed),
            |t| Some(&t.event_destroyed_parents),
            calendar_id,
        )?;
        // The account may own several calendars; the log mixes their
        // created / updated ids, so filter those down to this calendar.
        let live: HashMap<&str, &str> = self
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

    /// Per-account calendar delta. Drives the JMAP `Calendar/changes`
    /// walker (and the CalDAV MKCALENDAR / DELETE writes that feed it).
    /// Walks the account's own log, so every calendar id in it already
    /// belongs to the account.
    pub fn calendar_delta_since_account(&self, since: &str, account_id: &str) -> Option<DeltaSet> {
        self.walk_account(since, account_id, |t| {
            (
                &t.calendar_created,
                &t.calendar_updated,
                &t.calendar_destroyed,
            )
        })
    }

    /// Cross-calendar event delta for one account. Returns every event
    /// change since `since` regardless of which of the account's
    /// calendars it lives in. Drives JMAP `CalendarEvent/changes`,
    /// which carries no per-calendar filter on the wire (the
    /// `calendarIds` map on each event in the follow-up
    /// `CalendarEvent/get` is the per-resource scope).
    pub fn event_delta_since_any(&self, since: &str, account_id: &str) -> Option<DeltaSet> {
        self.walk_account(since, account_id, |t| {
            (&t.event_created, &t.event_updated, &t.event_destroyed)
        })
    }

    /// Contact-side analogue. Drives the Graph
    /// `/v1.0/me/contactFolders/{id}/contacts/delta` surface. Resolves
    /// the folder's owning account, then walks that account's log with
    /// the tombstone set scoped to the named folder via
    /// `contact_destroyed_parents`. (Created / updated stay
    /// account-wide, matching the pre-per-account behaviour - the Graph
    /// layer re-fetches per folder.) Tokens older than the seed (or
    /// evicted from the account's bounded ring) return `None`; the
    /// Graph layer translates that to a 410 Gone.
    pub fn contact_delta_since(&self, since: &str, folder_id: &str) -> Option<DeltaSet> {
        let account_id = match self.contact_folders.iter().find(|f| f.id == folder_id) {
            Some(f) => f.account_id.clone(),
            None => return self.seed_only_delta(since),
        };
        self.walk_account_filtered(
            since,
            &account_id,
            |t| (&t.contact_created, &t.contact_updated, &t.contact_destroyed),
            |t| Some(&t.contact_destroyed_parents),
            folder_id,
        )
    }

    /// Cross-folder contact delta for one account. Returns every
    /// contact change since `since` regardless of folder. Drives the
    /// People API listener, which has no folder concept (Google
    /// flattens every account contact into one connections list).
    pub fn contact_delta_since_any(&self, since: &str, account_id: &str) -> Option<DeltaSet> {
        self.walk_account(since, account_id, |t| {
            (&t.contact_created, &t.contact_updated, &t.contact_destroyed)
        })
    }

    /// Compute the full Gmail label set for an email: mailbox roles map
    /// to system labels (`INBOX` / `SENT` / `DRAFT` / `TRASH` / `SPAM`
    /// / `IMPORTANT`; `archive` is the absence of `INBOX`, not a
    /// label), `$seen`-absence -> `UNREAD`, `$flagged` -> `STARRED`,
    /// `$draft` -> `DRAFT`, and every non-`$` keyword -> `Label_<kw>`;
    /// a roleless mailbox contributes `Label_<mailbox-id>`. Sorted +
    /// deduped for determinism. The Gmail listener's `label_ids_for`
    /// delegates here so the read projection and the `history.list`
    /// label-delta capture agree byte-for-byte.
    pub fn gmail_label_ids(&self, email: &Email) -> Vec<String> {
        let by_id: HashMap<&str, &Mailbox> =
            self.mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut out = Vec::new();
        for mb_id in &email.mailbox_ids {
            let Some(m) = by_id.get(mb_id.as_str()) else {
                continue;
            };
            match m.role {
                Some(Role::Inbox) => out.push("INBOX".to_string()),
                Some(Role::Sent) => out.push("SENT".to_string()),
                Some(Role::Drafts) => out.push("DRAFT".to_string()),
                Some(Role::Trash) => out.push("TRASH".to_string()),
                Some(Role::Junk) => out.push("SPAM".to_string()),
                Some(Role::Important) => out.push("IMPORTANT".to_string()),
                Some(Role::Archive) => {}
                None => out.push(format!("Label_{}", m.id)),
            }
        }
        if !email.keywords.iter().any(|k| k == "$seen") {
            out.push("UNREAD".to_string());
        }
        if email.keywords.iter().any(|k| k == "$flagged") {
            out.push("STARRED".to_string());
        }
        if email.keywords.iter().any(|k| k == "$draft") {
            out.push("DRAFT".to_string());
        }
        for keyword in &email.keywords {
            if !keyword.starts_with('$') {
                out.push(format!("Label_{keyword}"));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Current Gmail `historyId` for `account_id`, as a `u64`. Mapped
    /// from the per-account change-log counter as `counter + 1` so a
    /// freshly-loaded fixture reports `1` (never `0`; real Gmail
    /// history ids are never zero and several tests pin the load-time
    /// value at `1`). Every `historyId` the Gmail listener emits
    /// (profile, thread / message bodies, the `history.list` top-level
    /// cursor) resolves through here, so a mutation on one account
    /// never moves a sibling account's Gmail cursor.
    pub fn gmail_history_id(&self, account_id: &str) -> u64 {
        self.account_logs
            .get(account_id)
            .map(|l| l.counter)
            .unwrap_or(0)
            + 1
    }

    /// Walk `account_id`'s change log for the Gmail `history.list`
    /// projection: one [`GmailHistoryRecord`] per transition newer than
    /// `start` (the client's `startHistoryId`, in the reported
    /// `counter + 1` space). Returns [`GmailHistory::Unknown`] when
    /// `start` predates the oldest retained transition (the bounded
    /// ring evicted it) so the caller can 404 and trigger a full
    /// re-sync, matching real Gmail. `start` at or past the current id
    /// yields an empty delta paired with the current id.
    pub fn gmail_history_since(&self, account_id: &str, start: u64) -> GmailHistory {
        // Map the reported id back to counter space (`reported =
        // counter + 1`). `start == 0` and `start == 1` both mean "from
        // the seed" (counter 0).
        let start = start.saturating_sub(1);
        let Some(log) = self.account_logs.get(account_id) else {
            // Never mutated: the seed (counter 0) is the only known
            // point. Anything newer can't be reconstructed.
            return if start == 0 {
                GmailHistory::Delta {
                    records: Vec::new(),
                    history_id: 1,
                }
            } else {
                GmailHistory::Unknown
            };
        };
        let current = log.counter;
        if start >= current {
            return GmailHistory::Delta {
                records: Vec::new(),
                history_id: current + 1,
            };
        }
        // `front_from` is the `from` counter of the oldest retained
        // transition; a `start` below it fell off the ring.
        let front_from = current - log.transitions.len() as u64;
        if start < front_from {
            return GmailHistory::Unknown;
        }
        let mut records = Vec::new();
        for (j, t) in log.transitions.iter().enumerate() {
            let to_counter = front_from + 1 + j as u64;
            if to_counter <= start {
                continue;
            }
            let labels_added = t
                .email_label_changes
                .iter()
                .filter(|c| !c.added.is_empty())
                .map(|c| (c.id.clone(), c.added.clone()))
                .collect();
            let labels_removed = t
                .email_label_changes
                .iter()
                .filter(|c| !c.removed.is_empty())
                .map(|c| (c.id.clone(), c.removed.clone()))
                .collect();
            records.push(GmailHistoryRecord {
                id: to_counter + 1,
                messages_added: t.email_created.clone(),
                messages_deleted: t.email_destroyed.clone(),
                labels_added,
                labels_removed,
            });
        }
        GmailHistory::Delta {
            records,
            history_id: current + 1,
        }
    }
}

/// Compute one email's Gmail label-set delta from its before / after
/// label sets. Returns `None` when nothing moved (so a body-only
/// update records no spurious `labelsAdded` / `labelsRemoved`).
pub fn gmail_label_change(id: &str, old: &[String], new: &[String]) -> Option<EmailLabelChange> {
    let added: Vec<String> = new.iter().filter(|l| !old.contains(l)).cloned().collect();
    let removed: Vec<String> = old.iter().filter(|l| !new.contains(l)).cloned().collect();
    if added.is_empty() && removed.is_empty() {
        None
    } else {
        Some(EmailLabelChange {
            id: id.to_string(),
            added,
            removed,
        })
    }
}

/// Result of [`Fixture::gmail_history_since`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GmailHistory {
    /// `start` predates the retained log; the caller must 404 so the
    /// client falls back to a full re-sync.
    Unknown,
    /// Records newer than `start` (possibly empty) plus the current
    /// top-level `historyId` to echo back.
    Delta {
        records: Vec<GmailHistoryRecord>,
        history_id: u64,
    },
}

/// One Gmail `history.list` record, projected from a single change-log
/// transition. `messages_added` / `messages_deleted` are email ids;
/// `labels_added` / `labels_removed` pair an email id with the Gmail
/// label ids that moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailHistoryRecord {
    pub id: u64,
    pub messages_added: Vec<String>,
    pub messages_deleted: Vec<String>,
    pub labels_added: Vec<(String, Vec<String>)>,
    pub labels_removed: Vec<(String, Vec<String>)>,
}

/// Build a [`Transition`] from a `MutationDiff` and its
/// `(from_state, to_state)` token pair, moving every id list across
/// and asserting the parallel destroyed-vectors stay aligned. Used
/// both for the per-account transitions pushed into each log and for
/// the cross-account aggregate `record_transition` returns.
fn transition_from_diff(from_state: String, to_state: String, diff: MutationDiff) -> Transition {
    let trans = Transition {
        from_state,
        to_state,
        email_created: diff.email_created,
        email_updated: diff.email_updated,
        email_destroyed: diff.email_destroyed,
        email_destroyed_accounts: diff.email_destroyed_accounts,
        email_label_changes: diff.email_label_changes,
        mailbox_created: diff.mailbox_created,
        mailbox_updated: diff.mailbox_updated,
        mailbox_destroyed: diff.mailbox_destroyed,
        mailbox_destroyed_accounts: diff.mailbox_destroyed_accounts,
        event_created: diff.event_created,
        event_updated: diff.event_updated,
        event_destroyed: diff.event_destroyed,
        event_destroyed_parents: diff.event_destroyed_parents,
        calendar_created: diff.calendar_created,
        calendar_updated: diff.calendar_updated,
        calendar_destroyed: diff.calendar_destroyed,
        calendar_destroyed_accounts: diff.calendar_destroyed_accounts,
        contact_created: diff.contact_created,
        contact_updated: diff.contact_updated,
        contact_destroyed: diff.contact_destroyed,
        contact_destroyed_parents: diff.contact_destroyed_parents,
        contact_folder_created: diff.contact_folder_created,
        contact_folder_updated: diff.contact_folder_updated,
        contact_folder_destroyed: diff.contact_folder_destroyed,
        contact_folder_destroyed_accounts: diff.contact_folder_destroyed_accounts,
        category_created: diff.category_created,
        category_updated: diff.category_updated,
        category_destroyed: diff.category_destroyed,
        category_destroyed_accounts: diff.category_destroyed_accounts,
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
    debug_assert_eq!(
        trans.email_destroyed.len(),
        trans.email_destroyed_accounts.len(),
        "email_destroyed_accounts must be parallel to email_destroyed"
    );
    debug_assert_eq!(
        trans.mailbox_destroyed.len(),
        trans.mailbox_destroyed_accounts.len(),
        "mailbox_destroyed_accounts must be parallel to mailbox_destroyed"
    );
    debug_assert_eq!(
        trans.calendar_destroyed.len(),
        trans.calendar_destroyed_accounts.len(),
        "calendar_destroyed_accounts must be parallel to calendar_destroyed"
    );
    debug_assert_eq!(
        trans.contact_folder_destroyed.len(),
        trans.contact_folder_destroyed_accounts.len(),
        "contact_folder_destroyed_accounts must be parallel to contact_folder_destroyed"
    );
    debug_assert_eq!(
        trans.category_destroyed.len(),
        trans.category_destroyed_accounts.len(),
        "category_destroyed_accounts must be parallel to category_destroyed"
    );
    trans
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

/// Account-discovery surface served on the JMAP HTTP listener.
///
/// Mirrors ratatoskr's multi-stage discovery cascade: WebFinger
/// (`/.well-known/webfinger`), OIDC discovery
/// (`/.well-known/openid-configuration`), and Mozilla autoconfig
/// (`/mail/config-v1.1.xml`). Each entry is keyed by the URL
/// path-prefix the request arrives at - ratatoskr's discovery
/// rewrites `https://{domain}/.well-known/...` to
/// `${BASE}/{domain}/.well-known/...`, so the prefix is typically
/// the bare domain (`corp.test`) but may be any sub-path
/// (`idp/realms/corp`) when a WebFinger response chained the
/// client onto a sub-issuer. See
/// `notes/ratatoskr-discovery-surface.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoveryConfig {
    pub entries: Vec<DiscoveryEntry>,
}

impl DiscoveryConfig {
    pub fn lookup(&self, prefix: &str) -> Option<&DiscoveryEntry> {
        self.entries.iter().find(|e| e.prefix == prefix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryEntry {
    /// Path prefix without surrounding slashes - the bit between
    /// the listener root and `/.well-known/...` (or the
    /// `/mail/config-v1.1.xml` suffix). Insertion-ordered.
    pub prefix: String,
    pub webfinger: Option<WebFingerDoc>,
    pub oidc: Option<OidcDoc>,
    pub autoconfig: Option<AutoconfigDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebFingerDoc {
    pub links: Vec<WebFingerLink>,
    /// Escape hatch: when present, the handler serves this body
    /// verbatim and ignores `links`. Drives malformed-JRD
    /// negative tests.
    pub raw_body: Option<String>,
    /// Overrides the default `application/jrd+json` content type.
    /// Used to assert ratatoskr tolerates `application/json` and
    /// rejects `text/html`.
    pub raw_content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFingerLink {
    pub rel: String,
    /// Absolute (`http://...` / `https://...`) -> emitted verbatim;
    /// path-relative (`/idp/realms/corp`) -> the handler prefixes
    /// the live listener base URL at emit time. Path-relative is
    /// the right default; the absolute form exists for negative
    /// tests (non-HTTPS scheme check).
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcDoc {
    /// Self-claimed issuer. Path-relative -> prefixed at emit
    /// time. Per OIDC Discovery 1.0 §4.3, must equal the request
    /// URL minus `/.well-known/openid-configuration`; the loader
    /// does NOT enforce that match so negative tests can stage
    /// the mismatch by setting the issuer to a different prefix.
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub raw_body: Option<String>,
    pub raw_content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoconfigDoc {
    /// Raw XML body. Substring `${BASE}` is replaced with the
    /// live listener base URL at emit time; other tokens are
    /// passed through verbatim. v0 leaves IMAP / SMTP port
    /// plumbing to the test author since ratatoskr's stage 2
    /// is not yet exercised against sæhrimnir.
    pub raw_body: String,
    /// Defaults to `application/xml` when None.
    pub raw_content_type: Option<String>,
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
    /// Account this calendar belongs to. See `Mailbox::account_id`.
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    /// Account this event belongs to. Derived from the parent
    /// calendar's account at load time; an event whose declared
    /// `account_id` (if any) disagrees with its calendar's
    /// account is rejected.
    pub account_id: String,
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
    /// RFC 5545 RRULE value (no `RRULE:` prefix - just the body,
    /// e.g. `"FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10"`). None means
    /// single-instance. CalDAV / gcal carry this verbatim; JMAP
    /// (JSCalendar) and Graph parse it into structured form via
    /// [`crate::recurrence::ParsedRule`].
    pub recurrence_rule: Option<String>,
    /// Per RFC 5545 EXDATE. Each entry renders as one EXDATE
    /// line / array entry in CalDAV / gcal and as a recurrence
    /// override (`recurrenceOverrides`) marker in JSCalendar /
    /// Graph in a later slice; for v0 reads the dates round-trip
    /// on CalDAV + gcal but are not emitted on JMAP / Graph (those
    /// schemas want a per-date override object the v0 fixture
    /// can't author yet). Empty by default.
    pub recurrence_exdates: Vec<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactFolder {
    pub id: String,
    /// Account this folder belongs to. See `Mailbox::account_id`.
    pub account_id: String,
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
    /// Account this contact belongs to. Derived from the parent
    /// folder's account at load time.
    pub account_id: String,
    pub folder_id: String,
    pub display_name: Option<String>,
    pub emails: Vec<ContactEmail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactEmail {
    pub address: String,
    pub name: Option<String>,
}

/// Outlook master category. Real Graph stores these flat on the
/// account; v0 mirrors that shape. `color` is a Graph preset
/// string ("preset0".."preset24" or "none"); the mock does not
/// validate the enum (real Graph accepts unknown presets too and
/// renders them as the default colour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub id: String,
    /// Account this category belongs to. See `Mailbox::account_id`.
    pub account_id: String,
    pub display_name: String,
    pub color: Option<String>,
}

/// Gmail SendAs identity. Mirrors the wire shape exposed under
/// `GET /gmail/v1/users/me/settings/sendAs` and accepted by the
/// `PATCH .../sendAs/{sendAsEmail}` mutation. Real Gmail tracks many
/// fields (`smtpMsa`, `verificationStatus`, etc.); v0 carries the
/// subset ratatoskr's sync code reads or writes. Mutations land
/// through a brief write guard on `Fixture` without bumping the
/// state token - Gmail does not expose a `/changes` endpoint for
/// sendAs, so harness scripts re-fetch to observe the update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendAs {
    /// Account this identity belongs to.
    pub account_id: String,
    /// `sendAsEmail` in the wire shape; the primary key per account.
    pub send_as_email: String,
    pub display_name: Option<String>,
    pub reply_to_address: Option<String>,
    /// HTML signature blob. Real Gmail accepts inline HTML; we
    /// pass it through unchanged.
    pub signature: Option<String>,
    pub is_primary: bool,
    pub is_default: bool,
    /// Real Gmail's `treatAsAlias` flag. Defaults to true for
    /// fixture-authored entries so the wire shape mirrors how real
    /// accounts present their primary identity.
    pub treat_as_alias: bool,
}

/// Microsoft Graph group. Distinct from the account-scoped
/// resources above: a group is a directory object whose `members`
/// list points at one or more declared accounts. The Graph
/// `Group` resource carries a lot of fields v0 doesn't model
/// (groupTypes, classifications, ...); the projection emits a
/// minimal `{ id, displayName, description, mail, mailEnabled,
/// securityEnabled, members[] }` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub mail: Option<String>,
    pub mail_enabled: bool,
    pub security_enabled: bool,
    /// Account ids that belong to the group. Validated at load
    /// time against the declared `[[account]]` set.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// The v0 account every protocol surface scopes to today.
    /// Exactly one declared account is the primary. Single-account
    /// fixtures (the v0 majority) get their lone account flagged
    /// primary automatically by the loader.
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub id: String,
    /// Account this mailbox belongs to. Defaults to the primary
    /// account when the fixture omits the field. Stage 2 of the
    /// multi-account refactor introduces this field; per-protocol
    /// scoping (Stage 3) lets handlers serve non-primary accounts.
    pub account_id: String,
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
    /// Account this email belongs to. See `Mailbox::account_id`.
    pub account_id: String,
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

/// Structured fields projected from an uploaded / sent RFC 822 message.
/// Shared by the JMAP `Email/import` and Gmail `messages.send` delivery
/// paths so a client send lands in the fixture with real metadata.
#[derive(Debug, Clone, Default)]
pub struct ParsedRfc822 {
    pub subject: Option<String>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub message_id: Vec<String>,
    pub body_text: Option<String>,
}

/// Project an RFC 822 message into [`ParsedRfc822`]. Best-effort:
/// anything `mail-parser` can't read is left empty, so a delivery path
/// stays total (a malformed message still lands, just sparser). Only
/// the first `From` / `To` address is captured (the fixture `Email`'s
/// envelope fidelity is coarse in v0).
pub fn parse_rfc822_email(raw: &[u8]) -> ParsedRfc822 {
    let parser = mail_parser::MessageParser::default();
    let Some(msg) = parser.parse(raw) else {
        return ParsedRfc822::default();
    };
    let address_of = |addr: &mail_parser::Addr| Address {
        name: addr.name.as_ref().map(std::string::ToString::to_string),
        email: addr
            .address
            .as_ref()
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
    };
    let from = msg.from().and_then(|a| a.first()).map(address_of);
    let to = msg
        .to()
        .and_then(|a| a.first())
        .map(|a| vec![address_of(a)])
        .unwrap_or_default();
    let message_id = msg
        .message_id()
        .map(|m| vec![m.to_string()])
        .unwrap_or_default();
    ParsedRfc822 {
        subject: msg.subject().map(std::string::ToString::to_string),
        from,
        to,
        message_id,
        body_text: msg.body_text(0).map(std::borrow::Cow::into_owned),
    }
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
    /// `[[account]]` repeated. At least one entry; exactly one
    /// flagged `primary = true` (or, when none is flagged, the
    /// first entry is taken as primary so single-account fixtures
    /// don't have to bother). All entries must have
    /// `is_personal = true` for the duration of Stage 1; relaxes
    /// once Graph shared mailbox sync lands.
    #[serde(default, rename = "account")]
    pub(crate) accounts: Vec<RawAccount>,
    #[serde(default, rename = "mailbox")]
    pub(crate) mailboxes: Vec<RawMailbox>,
    #[serde(default, rename = "email")]
    pub(crate) emails: Vec<RawEmail>,
    #[serde(default)]
    pub(crate) oauth: Option<RawOAuth>,
    /// `[discovery."prefix"]` table. Iteration order matters
    /// (preserved insertion order via `toml::Table`) so the
    /// emitted fixture is byte-deterministic; the runtime is
    /// keyed by prefix anyway, so the order only affects
    /// debug-print ordering and the lua-vs-toml equivalence
    /// check.
    #[serde(default)]
    pub(crate) discovery: std::collections::BTreeMap<String, RawDiscoveryEntry>,
    #[serde(default, rename = "calendar")]
    pub(crate) calendars: Vec<RawCalendar>,
    #[serde(default, rename = "event")]
    pub(crate) events: Vec<RawEvent>,
    #[serde(default, rename = "contact_folder")]
    pub(crate) contact_folders: Vec<RawContactFolder>,
    #[serde(default, rename = "contact")]
    pub(crate) contacts: Vec<RawContact>,
    #[serde(default, rename = "category")]
    pub(crate) categories: Vec<RawCategory>,
    #[serde(default, rename = "group")]
    pub(crate) groups: Vec<RawGroup>,
    #[serde(default, rename = "send_as")]
    pub(crate) send_as: Vec<RawSendAs>,
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
    #[serde(default)]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawContact {
    pub(crate) id: String,
    pub(crate) folder_id: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) emails: Vec<RawContactEmail>,
    /// Optional override; defaults to the parent folder's
    /// `account_id` and must match it when both are present.
    #[serde(default)]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawCategory {
    pub(crate) id: String,
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) color: Option<String>,
    #[serde(default)]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSendAs {
    pub(crate) send_as_email: String,
    #[serde(default)]
    pub(crate) account_id: Option<String>,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) reply_to_address: Option<String>,
    #[serde(default)]
    pub(crate) signature: Option<String>,
    #[serde(default)]
    pub(crate) is_primary: Option<bool>,
    #[serde(default)]
    pub(crate) is_default: Option<bool>,
    #[serde(default)]
    pub(crate) treat_as_alias: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawGroup {
    pub(crate) id: String,
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) mail: Option<String>,
    #[serde(default)]
    pub(crate) mail_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) security_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) members: Vec<String>,
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
            RawContactEmail::Bare(address) => Self {
                address,
                name: None,
            },
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
    #[serde(default)]
    pub(crate) account_id: Option<String>,
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
    /// Raw RRULE value (no `RRULE:` prefix). Optional.
    #[serde(default)]
    pub(crate) recurrence_rule: Option<String>,
    /// Excluded recurrence dates. Each entry is an RFC 3339
    /// timestamp; parsed via `parse_ts` at normalize time.
    #[serde(default)]
    pub(crate) recurrence_exdates: Vec<String>,
    /// Optional override. If absent the event inherits its parent
    /// calendar's `account_id`; if present it must match the
    /// parent's account or the loader rejects the cross-account
    /// pairing.
    #[serde(default)]
    pub(crate) account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawOAuth {
    #[serde(default)]
    pub(crate) enforce: bool,
    #[serde(default)]
    pub(crate) issuer: Option<String>,
}

/// TOML form: `[discovery."prefix"]` map. The bare table is the
/// path-prefix key; nested `webfinger` / `oidc` / `autoconfig`
/// are the per-shape docs. Authoring shape mirrors the user-
/// proposal:
/// ```toml
/// [discovery."corp.test"]
/// webfinger.links = [
///   { rel = "...", href = "/idp/realms/corp" },
/// ]
///
/// [discovery."idp/realms/corp".oidc]
/// issuer = "/idp/realms/corp"
/// authorization_endpoint = "/oauth/authorize"
/// token_endpoint = "/oauth/token"
/// scopes_supported = ["openid", "email"]
/// code_challenge_methods_supported = ["S256"]
/// token_endpoint_auth_methods_supported = ["none"]
/// ```
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawDiscoveryEntry {
    #[serde(default)]
    pub(crate) webfinger: Option<RawWebFinger>,
    #[serde(default)]
    pub(crate) oidc: Option<RawOidc>,
    #[serde(default)]
    pub(crate) autoconfig: Option<RawAutoconfig>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawWebFinger {
    #[serde(default)]
    pub(crate) links: Vec<RawWebFingerLink>,
    #[serde(default)]
    pub(crate) raw_body: Option<String>,
    #[serde(default)]
    pub(crate) raw_content_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawWebFingerLink {
    pub(crate) rel: String,
    pub(crate) href: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawOidc {
    pub(crate) issuer: String,
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    #[serde(default)]
    pub(crate) userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) scopes_supported: Vec<String>,
    #[serde(default)]
    pub(crate) code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    pub(crate) token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default)]
    pub(crate) raw_body: Option<String>,
    #[serde(default)]
    pub(crate) raw_content_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawAutoconfig {
    pub(crate) raw_body: String,
    #[serde(default)]
    pub(crate) raw_content_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawAccount {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) is_personal: bool,
    /// Marks this entry as the v0 protocol-scoped account when the
    /// fixture declares more than one. Optional; for single-account
    /// fixtures the loader flags the lone entry primary
    /// automatically. Exactly one declared account may be primary.
    #[serde(default)]
    pub(crate) primary: bool,
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
    /// Account this mailbox belongs to. Defaults to the primary
    /// account at normalize time when absent.
    #[serde(default)]
    pub(crate) account_id: Option<String>,
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
    /// Account this email belongs to. Defaults to primary when
    /// absent. Must agree with the account of every mailbox
    /// listed in `mailbox_ids`; the loader rejects cross-account
    /// references.
    #[serde(default)]
    pub(crate) account_id: Option<String>,
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
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
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
    if raw.accounts.is_empty() {
        return Err("fixture must declare at least one `[[account]]`".into());
    }
    // Reject duplicate account ids before any other validation so
    // the error message is consistent regardless of which other
    // invariants the fixture violates.
    {
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for a in &raw.accounts {
            if seen.insert(a.id.as_str(), ()).is_some() {
                return Err(format!("duplicate account id {:?}", a.id));
            }
        }
    }
    // Promote the lone account in single-account fixtures to primary
    // automatically so authors don't have to write `primary = true`
    // boilerplate. Multi-account fixtures must mark one explicitly;
    // ambiguity gets a clear error rather than a silent first-wins.
    let primary_flags = raw.accounts.iter().filter(|a| a.primary).count();
    let mut accounts_raw = raw.accounts;
    if primary_flags == 0 {
        if accounts_raw.len() == 1 {
            accounts_raw[0].primary = true;
        } else {
            return Err("fixture declares multiple accounts but none is `primary = true`".into());
        }
    } else if primary_flags > 1 {
        return Err(format!(
            "fixture declares {primary_flags} primary accounts; exactly one may be primary"
        ));
    }
    // Stage 1 invariants: every account must be personal and have
    // an email-shaped name (OAuth userinfo serves `account.name`
    // as the `email` / `name` claims).
    let mut accounts: Vec<Account> = Vec::with_capacity(accounts_raw.len());
    for raw_acct in accounts_raw {
        if !raw_acct.is_personal {
            return Err(format!(
                "account {:?}: is_personal must be true (Stage 1 of the multi-account refactor still requires every declared account to be personal)",
                raw_acct.id
            ));
        }
        if !is_email_shaped(&raw_acct.name) {
            return Err(format!(
                "account {:?}: name must be email-shaped (got {:?}); the OAuth userinfo endpoint exposes it as the `email` claim",
                raw_acct.id, raw_acct.name
            ));
        }
        accounts.push(Account {
            id: raw_acct.id,
            name: raw_acct.name,
            primary: raw_acct.primary,
        });
    }

    let primary_id = accounts
        .iter()
        .find(|a| a.primary)
        .map(|a| a.id.clone())
        .expect("primary guaranteed by primary_flags check above");
    let account_ids: HashMap<String, ()> = accounts.iter().map(|a| (a.id.clone(), ())).collect();
    let resolve_account = |raw: Option<&String>,
                           resource: &str,
                           id: &str|
     -> Result<String, String> {
        match raw {
            Some(a) => {
                if !account_ids.contains_key(a) {
                    return Err(format!(
                        "{resource} {id:?}: account_id {a:?} does not match any declared account"
                    ));
                }
                Ok(a.clone())
            }
            None => Ok(primary_id.clone()),
        }
    };

    let mut mb_ids: HashMap<String, ()> = HashMap::new();
    for mb in &raw.mailboxes {
        if mb_ids.insert(mb.id.clone(), ()).is_some() {
            return Err(format!("duplicate mailbox id {:?}", mb.id));
        }
    }

    // mailbox id -> account_id, used downstream to validate that
    // every email's listed mailboxes share its account and to seed
    // each Email's `account_id` from its primary mailbox.
    let mut mb_accounts: HashMap<String, String> = HashMap::new();
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
        let acct = resolve_account(mb.account_id.as_ref(), "mailbox", &mb.id)?;
        mb_accounts.insert(mb.id.clone(), acct.clone());
        mailboxes.push(Mailbox {
            is_subscribed: mb.is_subscribed.unwrap_or(true),
            id: mb.id,
            account_id: acct,
            name: mb.name,
            role,
            parent_id: mb.parent_id,
            sort_order: mb.sort_order,
        });
    }
    detect_cycles(&mailboxes)?;
    // A child mailbox must share an account with its parent;
    // cross-account nesting has no real-world analogue and would
    // confuse the UID history walk.
    for mb in &mailboxes {
        if let Some(parent) = &mb.parent_id
            && let Some(parent_acct) = mb_accounts.get(parent)
            && parent_acct != &mb.account_id
        {
            return Err(format!(
                "mailbox {:?}: account_id {:?} disagrees with parent {parent:?} on account {parent_acct:?}",
                mb.id, mb.account_id
            ));
        }
    }

    let mut email_ids: HashMap<String, ()> = HashMap::new();
    for em in &raw.emails {
        if email_ids.insert(em.id.clone(), ()).is_some() {
            return Err(format!("duplicate email id {:?}", em.id));
        }
    }

    let mut emails = Vec::with_capacity(raw.emails.len());
    for em in raw.emails {
        let email = normalize_email(
            em,
            &mb_ids,
            Some(&mb_accounts),
            Some(&account_ids),
            fixture_dir,
        )?;
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

    let discovery = normalize_discovery(raw.discovery)?;

    // Calendars and events. Validate that every event references a
    // declared calendar - same shape as the email -> mailbox check.
    let mut calendar_ids: HashMap<String, ()> = HashMap::new();
    let mut calendar_accounts: HashMap<String, String> = HashMap::new();
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
        let acct = resolve_account(cal.account_id.as_ref(), "calendar", &cal.id)?;
        calendar_accounts.insert(cal.id.clone(), acct.clone());
        calendars.push(Calendar {
            id: cal.id,
            account_id: acct,
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
        let parent_acct = calendar_accounts
            .get(&ev.calendar_id)
            .expect("calendar existence checked above")
            .clone();
        if let Some(declared) = &ev.account_id {
            if !account_ids.contains_key(declared) {
                return Err(format!(
                    "event {:?}: account_id {declared:?} does not match any declared account",
                    ev.id
                ));
            }
            if declared != &parent_acct {
                return Err(format!(
                    "event {:?}: declared account_id {declared:?} disagrees with calendar's account {parent_acct:?}",
                    ev.id
                ));
            }
        }
        let start = parse_ts(&ev.start).map_err(|e| format!("event {:?} start: {e}", ev.id))?;
        let end = parse_ts(&ev.end).map_err(|e| format!("event {:?} end: {e}", ev.id))?;
        let mut recurrence_exdates: Vec<DateTime<Utc>> =
            Vec::with_capacity(ev.recurrence_exdates.len());
        for s in &ev.recurrence_exdates {
            recurrence_exdates.push(
                parse_ts(s).map_err(|e| format!("event {:?} recurrence_exdates: {e}", ev.id))?,
            );
        }
        events.push(Event {
            id: ev.id,
            account_id: parent_acct,
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
            recurrence_rule: ev.recurrence_rule,
            recurrence_exdates,
        });
    }

    // Contact folders + contacts. Same shape as calendars + events:
    // every contact references a declared folder, ids are unique
    // within their kind.
    let mut contact_folder_ids: HashMap<String, ()> = HashMap::new();
    let mut contact_folder_accounts: HashMap<String, String> = HashMap::new();
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
        let acct = resolve_account(cf.account_id.as_ref(), "contact_folder", &cf.id)?;
        if let Some(parent) = &cf.parent_folder_id
            && let Some(parent_acct) = contact_folder_accounts.get(parent)
            && parent_acct != &acct
        {
            return Err(format!(
                "contact_folder {:?}: account {acct:?} disagrees with parent {parent:?} on account {parent_acct:?}",
                cf.id
            ));
        }
        contact_folder_accounts.insert(cf.id.clone(), acct.clone());
        contact_folders.push(ContactFolder {
            id: cf.id,
            account_id: acct,
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
        let parent_acct = contact_folder_accounts
            .get(&c.folder_id)
            .expect("folder existence checked above")
            .clone();
        if let Some(declared) = &c.account_id {
            if !account_ids.contains_key(declared) {
                return Err(format!(
                    "contact {:?}: account_id {declared:?} does not match any declared account",
                    c.id
                ));
            }
            if declared != &parent_acct {
                return Err(format!(
                    "contact {:?}: declared account_id {declared:?} disagrees with folder's account {parent_acct:?}",
                    c.id
                ));
            }
        }
        contacts.push(Contact {
            id: c.id,
            account_id: parent_acct,
            folder_id: c.folder_id,
            display_name: c.display_name,
            emails: c.emails.into_iter().map(ContactEmail::from).collect(),
        });
    }

    // Microsoft Graph groups. Cross-account: members reference
    // declared `[[account]]` ids. Distinct id namespace from
    // every other resource type.
    let mut group_ids: HashMap<String, ()> = HashMap::new();
    let mut groups = Vec::with_capacity(raw.groups.len());
    for grp in raw.groups {
        if group_ids.insert(grp.id.clone(), ()).is_some() {
            return Err(format!("duplicate group id {:?}", grp.id));
        }
        // Member ids must each match a declared account; duplicates
        // within one group are rejected so the wire projection of
        // /groups/{id}/members stays a set.
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for m in &grp.members {
            if !account_ids.contains_key(m) {
                return Err(format!(
                    "group {:?}: member {m:?} does not match any declared account",
                    grp.id
                ));
            }
            if seen.insert(m.as_str(), ()).is_some() {
                return Err(format!("group {:?}: duplicate member {m:?}", grp.id));
            }
        }
        groups.push(Group {
            id: grp.id,
            display_name: grp.display_name,
            description: grp.description,
            mail: grp.mail,
            mail_enabled: grp.mail_enabled.unwrap_or(false),
            security_enabled: grp.security_enabled.unwrap_or(false),
            members: grp.members,
        });
    }

    // SendAs identities. Flat per-account; primary key per account
    // is the `send_as_email` address. Real Gmail accepts the same
    // address under multiple accounts (e.g. an alias shared across
    // mailboxes), so we scope the uniqueness check to
    // `(account_id, send_as_email)`.
    let mut send_as_keys: HashMap<(String, String), ()> = HashMap::new();
    let mut send_as_entries = Vec::with_capacity(raw.send_as.len());
    for sa in raw.send_as {
        let acct = resolve_account(sa.account_id.as_ref(), "send_as", &sa.send_as_email)?;
        if send_as_keys
            .insert((acct.clone(), sa.send_as_email.clone()), ())
            .is_some()
        {
            return Err(format!(
                "duplicate send_as {:?} on account {:?}",
                sa.send_as_email, acct,
            ));
        }
        send_as_entries.push(SendAs {
            account_id: acct,
            send_as_email: sa.send_as_email,
            display_name: sa.display_name,
            reply_to_address: sa.reply_to_address,
            signature: sa.signature,
            is_primary: sa.is_primary.unwrap_or(false),
            is_default: sa.is_default.unwrap_or(false),
            treat_as_alias: sa.treat_as_alias.unwrap_or(true),
        });
    }

    // Master categories. Flat list per account, ids unique.
    let mut category_ids: HashMap<String, ()> = HashMap::new();
    let mut categories = Vec::with_capacity(raw.categories.len());
    for cat in raw.categories {
        if category_ids.insert(cat.id.clone(), ()).is_some() {
            return Err(format!("duplicate category id {:?}", cat.id));
        }
        let acct = resolve_account(cat.account_id.as_ref(), "category", &cat.id)?;
        categories.push(Category {
            id: cat.id,
            account_id: acct,
            display_name: cat.display_name,
            color: cat.color,
        });
    }

    // Change-script projection. Normalised against the *baseline*
    // mailbox set declared in the same fixture; mailboxes that a
    // later step's `mailbox_create` op introduces are not visible
    // here (matching the Lua loader's snapshot semantics, see
    // `src/lua.rs::read_email_create`).
    let mut change_script: Vec<ChangeStep> = Vec::with_capacity(raw.change_script.len());
    for raw_step in raw.change_script {
        change_script.push(normalize_change_step(
            raw_step,
            &mb_ids,
            &mb_accounts,
            &account_ids,
            fixture_dir,
        )?);
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

    let state_seed = raw.state.unwrap_or_else(|| "fixture-state".to_string());
    // Seed the counter to the larger of (a) the highest declared
    // `mock-X-N` suffix and (b) the loaded resource count. (a) is
    // the strict collision-avoidance guarantee. (b) preserves the
    // pre-fix wire shape: a fixture with two `email-001` /
    // `email-002` declared emails (no `mock-email-` ids) used to
    // mint `mock-email-3` because the old code did `len() + 1`.
    // Tests pin that value, so the counter starts at len when no
    // mock-prefixed declarations beat it.
    let synthetic_event_seq =
        max_mock_seq(events.iter().map(|e| e.id.as_str()), "mock-event-").max(events.len() as u64);
    let synthetic_email_seq =
        max_mock_seq(emails.iter().map(|e| e.id.as_str()), "mock-email-").max(emails.len() as u64);
    let synthetic_category_seq =
        max_mock_seq(categories.iter().map(|c| c.id.as_str()), "mock-category-")
            .max(categories.len() as u64);
    let synthetic_contact_seq =
        max_mock_seq(contacts.iter().map(|c| c.id.as_str()), "mock-contact-")
            .max(contacts.len() as u64);
    Ok(Fixture {
        name: raw.name,
        state_seed,
        accounts,
        mailboxes,
        emails,
        oauth,
        discovery,
        calendars,
        events,
        contact_folders,
        contacts,
        categories,
        groups,
        send_as: send_as_entries,
        account_logs: BTreeMap::new(),
        change_script,
        mailbox_uid_history,
        synthetic_event_seq,
        synthetic_email_seq,
        synthetic_category_seq,
        synthetic_contact_seq,
        uploaded_blobs: BTreeMap::new(),
        gmail_label_colors: BTreeMap::new(),
    })
}

/// Normalise the raw `[discovery."prefix"]` map into the typed
/// `DiscoveryConfig`. Validation is intentionally light: prefixes
/// must be non-empty and free of surrounding slashes (so the
/// runtime lookup against the URL-derived prefix is a plain
/// string compare); per-doc shape is otherwise pass-through
/// since negative tests rely on staging deliberately-broken
/// docs (issuer mismatch, http:// href, malformed JRD).
fn normalize_discovery(
    raw: std::collections::BTreeMap<String, RawDiscoveryEntry>,
) -> Result<DiscoveryConfig, String> {
    let mut entries = Vec::with_capacity(raw.len());
    for (prefix, entry) in raw {
        if prefix.is_empty() {
            return Err("discovery prefix must be non-empty".to_string());
        }
        if prefix.starts_with('/') || prefix.ends_with('/') {
            return Err(format!(
                "discovery prefix {prefix:?} must not start or end with '/'"
            ));
        }
        let webfinger = entry.webfinger.map(|w| WebFingerDoc {
            links: w
                .links
                .into_iter()
                .map(|l| WebFingerLink {
                    rel: l.rel,
                    href: l.href,
                })
                .collect(),
            raw_body: w.raw_body,
            raw_content_type: w.raw_content_type,
        });
        let oidc = entry.oidc.map(|o| OidcDoc {
            issuer: o.issuer,
            authorization_endpoint: o.authorization_endpoint,
            token_endpoint: o.token_endpoint,
            userinfo_endpoint: o.userinfo_endpoint,
            scopes_supported: o.scopes_supported,
            code_challenge_methods_supported: o.code_challenge_methods_supported,
            token_endpoint_auth_methods_supported: o.token_endpoint_auth_methods_supported,
            raw_body: o.raw_body,
            raw_content_type: o.raw_content_type,
        });
        let autoconfig = entry.autoconfig.map(|a| AutoconfigDoc {
            raw_body: a.raw_body,
            raw_content_type: a.raw_content_type,
        });
        entries.push(DiscoveryEntry {
            prefix,
            webfinger,
            oidc,
            autoconfig,
        });
    }
    Ok(DiscoveryConfig { entries })
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
        // Empty string here signals "default to primary at apply
        // time"; the applier in `test_admin.rs` substitutes the
        // live fixture's primary account id. A fixture-authored
        // `account_id` flows through verbatim; the applier
        // validates the ref.
        account_id: raw.account_id.unwrap_or_default(),
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
    let mut recurrence_exdates: Vec<DateTime<Utc>> =
        Vec::with_capacity(raw.recurrence_exdates.len());
    for s in &raw.recurrence_exdates {
        recurrence_exdates
            .push(parse_ts(s).map_err(|e| format!("{id_for_msg:?} recurrence_exdates: {e}"))?);
    }
    Ok(ChangeOp::EventCreate(Box::new(Event {
        id: raw.id,
        // See `mailbox_create_op` for the empty-string semantics.
        account_id: raw.account_id.unwrap_or_default(),
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
        recurrence_rule: raw.recurrence_rule,
        recurrence_exdates,
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
        account_id: raw.account_id.unwrap_or_default(),
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
        patch.insert("parent_folder_id".into(), serde_json::Value::String(p));
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
        account_id: raw.account_id.unwrap_or_default(),
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
    mb_accounts: &HashMap<String, String>,
    account_ids: &HashMap<String, ()>,
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
        let email = normalize_email(
            em,
            mb_ids,
            Some(mb_accounts),
            Some(account_ids),
            fixture_dir,
        )
        .map_err(|e| format!("change step {id:?}: email_create entry {email_id:?}: {e}"))?;
        ops.push(ChangeOp::EmailCreate(Box::new(email)));
    }
    for u in raw.email_update {
        let entry_id = u.id.clone();
        ops.push(
            email_update_op(u.id, u.keywords, u.mailbox_ids)
                .map_err(|e| format!("change step {id:?}: email_update entry {entry_id:?}: {e}"))?,
        );
    }
    for m in raw.email_move {
        let entry_id = m.id.clone();
        ops.push(
            email_move_op(m.id, m.mailbox_ids)
                .map_err(|e| format!("change step {id:?}: email_move entry {entry_id:?}: {e}"))?,
        );
    }
    for d in raw.email_destroy {
        ops.push(ChangeOp::EmailDestroy { id: d });
    }

    for mb in raw.mailbox_create {
        let entry_id = mb.id.clone();
        ops.push(
            mailbox_create_op(mb)
                .map_err(|e| format!("change step {id:?}: mailbox_create {entry_id:?}: {e}"))?,
        );
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
            .map_err(|e| format!("change step {id:?}: mailbox_update entry {entry_id:?}: {e}"))?,
        );
    }
    for d in raw.mailbox_destroy {
        ops.push(ChangeOp::MailboxDestroy { id: d });
    }

    for ev in raw.event_create {
        ops.push(event_create_op(ev).map_err(|e| format!("change step {id:?}: event_create {e}"))?);
    }
    for u in raw.event_update {
        let entry_id = u.id.clone();
        ops.push(
            event_update_op(u.id, u.subject, u.start, u.end, u.location, u.body_text)
                .map_err(|e| format!("change step {id:?}: event_update entry {entry_id:?}: {e}"))?,
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
            contact_folder_update_op(u.id, u.display_name, u.parent_folder_id).map_err(|e| {
                format!("change step {id:?}: contact_folder_update entry {entry_id:?}: {e}")
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
// `mb_accounts: None` skips account derivation entirely (the
// resulting email gets `account_id = String::new()`, an apply-time
// placeholder). The change-script Lua path uses None because it
// doesn't yet have a resolved account context at load time; the
// applier in `test_admin.rs` fills the placeholder in.
pub(crate) fn normalize_email(
    em: RawEmail,
    mb_ids: &HashMap<String, ()>,
    mb_accounts: Option<&HashMap<String, String>>,
    account_ids: Option<&HashMap<String, ()>>,
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

    // Derive the email's account from its mailboxes (which carry
    // the authoritative account_id). All listed mailboxes must
    // share one account: an email straddling accounts would have
    // no meaningful Graph `/users/{id}/messages/{id}` parent and
    // no JMAP single accountId to surface under. If the fixture
    // sets `email.account_id` explicitly, validate it matches.
    let derived_account: String = if let Some(mb_accounts) = mb_accounts {
        let mut iter = em.mailbox_ids.iter();
        let first_mb = iter.next().expect("non-empty checked above");
        let first_acct = mb_accounts
            .get(first_mb)
            .expect("mailbox existence checked above")
            .clone();
        for mid in iter {
            let other = mb_accounts.get(mid).expect("checked");
            if other != &first_acct {
                return Err(format!(
                    "email {:?}: mailboxes {first_mb:?} and {mid:?} live in different accounts ({first_acct:?} vs {other:?})",
                    em.id
                ));
            }
        }
        first_acct
    } else {
        // Apply-time placeholder; the change-script applier in
        // `test_admin.rs` resolves it against the live fixture.
        String::new()
    };
    if let Some(declared) = &em.account_id
        && let Some(account_ids) = account_ids
    {
        if !account_ids.contains_key(declared) {
            return Err(format!(
                "email {:?}: account_id {declared:?} does not match any declared account",
                em.id
            ));
        }
        if !derived_account.is_empty() && declared != &derived_account {
            return Err(format!(
                "email {:?}: declared account_id {declared:?} disagrees with mailbox-derived account {derived_account:?}",
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
        account_id: derived_account,
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
    let by_id: HashMap<&str, &Mailbox> = mailboxes.iter().map(|m| (m.id.as_str(), m)).collect();
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

        [[account]]
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
        assert_eq!(fix.primary_account().id, "a1");
        assert_eq!(fix.accounts.len(), 1);
        assert!(fix.accounts[0].primary);
        assert_eq!(fix.mailboxes.len(), 1);
        assert_eq!(fix.mailboxes[0].role, Some(Role::Inbox));
        assert!(fix.mailboxes[0].is_subscribed);
        assert_eq!(fix.emails.len(), 1);
        assert_eq!(fix.emails[0].thread_id, "e1");
        assert_eq!(fix.emails[0].mailbox_ids, vec!["mb-inbox".to_string()]);
        assert_eq!(fix.emails[0].sent_at, fix.emails[0].received_at);
        assert!(matches!(&fix.emails[0].body, Body::Text(t) if t == "hello"));
        assert_eq!(fix.emails[0].size, 5);
        assert_eq!(
            fix.emails[0].from.as_ref().unwrap().email,
            "alice@example.com"
        );
        assert!(fix.emails[0].from.as_ref().unwrap().name.is_none());
    }

    #[test]
    fn defaults_state_token() {
        let fix = parse(MINIMAL).unwrap();
        assert_eq!(fix.state_seed, "fixture-state");
        assert_eq!(fix.primary_state(), "fixture-state");
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
            [[account]]
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
    fn multi_account_promotes_explicit_primary() {
        let s = r#"
            name = "ma"
            [[account]]
            id = "a1"
            name = "one@example.com"
            is_personal = true
            primary = true
            [[account]]
            id = "a2"
            name = "two@example.com"
            is_personal = true
        "#;
        let fix = parse(s).unwrap();
        assert_eq!(fix.accounts.len(), 2);
        assert_eq!(fix.primary_account().id, "a1");
        assert!(!fix.accounts.iter().find(|a| a.id == "a2").unwrap().primary);
    }

    #[test]
    fn multi_account_without_explicit_primary_is_rejected() {
        let s = r#"
            name = "ma"
            [[account]]
            id = "a1"
            name = "one@example.com"
            is_personal = true
            [[account]]
            id = "a2"
            name = "two@example.com"
            is_personal = true
        "#;
        let err = parse(s).unwrap_err();
        assert!(err.contains("primary"), "got: {err}");
    }

    #[test]
    fn multi_account_with_two_primaries_is_rejected() {
        let s = r#"
            name = "ma"
            [[account]]
            id = "a1"
            name = "one@example.com"
            is_personal = true
            primary = true
            [[account]]
            id = "a2"
            name = "two@example.com"
            is_personal = true
            primary = true
        "#;
        let err = parse(s).unwrap_err();
        assert!(err.contains("primary"), "got: {err}");
    }

    #[test]
    fn duplicate_account_id_rejected() {
        let s = r#"
            name = "ma"
            [[account]]
            id = "a1"
            name = "one@example.com"
            is_personal = true
            [[account]]
            id = "a1"
            name = "two@example.com"
            is_personal = true
        "#;
        let err = parse(s).unwrap_err();
        assert!(err.contains("duplicate account id"), "got: {err}");
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
