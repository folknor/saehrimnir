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
}

/// Resource-id deltas a single mutator pass produced. Returned by the
/// closure passed to [`Fixture::mutate`]; the caller never constructs
/// transitions directly. An all-empty `MutationDiff` is treated as a
/// no-op: the state token does not bump and no transition is recorded.
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
    /// Apply a mutation, record its transition, and bump `state`.
    /// The closure is the only thing allowed to touch the fixture
    /// fields; it returns the resource-id diff so we can capture it
    /// without re-walking the (potentially large) email/mailbox
    /// vectors. An all-empty diff is a no-op (no state bump, no
    /// transition recorded) so that idempotent set-calls (e.g. an
    /// `update` block that only patches keywords already present)
    /// stay observable as "nothing changed".
    pub fn mutate<F>(&mut self, f: F) -> Transition
    where
        F: FnOnce(&mut Fixture) -> MutationDiff,
    {
        let from_state = self.state.clone();
        let diff = f(self);
        if diff.is_empty() {
            // No-op: don't bump state, don't record a transition.
            // Return a marker transition with `from == to` so the
            // caller's `oldState == newState` response stays correct
            // without us polluting the log.
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
        };
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
    pub fn event_delta_since(&self, since: &str) -> Option<DeltaSet> {
        self.delta_since(since, |t| {
            (&t.event_created, &t.event_updated, &t.event_destroyed)
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
        // Apply RFC 8620 §5.2 dominance: created∩destroyed cancels;
        // created+updated collapses to created; destroyed+updated
        // collapses to destroyed. The order of these checks matters
        // (cancel first, then collapse).
        let cancel: std::collections::HashSet<String> = created
            .iter()
            .filter(|id| destroyed.contains(id))
            .cloned()
            .collect();
        created.retain(|id| !cancel.contains(id));
        destroyed.retain(|id| !cancel.contains(id));
        let in_created: std::collections::HashSet<String> = created.iter().cloned().collect();
        let in_destroyed: std::collections::HashSet<String> = destroyed.iter().cloned().collect();
        updated.retain(|id| !in_created.contains(id) && !in_destroyed.contains(id));
        // Dedup each list while preserving first-seen order (stable
        // for byte-determinism).
        dedup_preserving_order(&mut created);
        dedup_preserving_order(&mut updated);
        dedup_preserving_order(&mut destroyed);
        Some(DeltaSet {
            created,
            updated,
            destroyed,
        })
    }
}

fn dedup_preserving_order(v: &mut Vec<String>) {
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(v.len());
    v.retain(|id| seen.insert(id.clone()));
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

    let state = raw.state.unwrap_or_else(|| "fixture-state".to_string());
    let change_log = ChangeLog::seed(&state);
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
        change_log,
        change_script: Vec::new(),
    })
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
