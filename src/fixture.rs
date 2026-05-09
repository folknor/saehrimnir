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

    fn parse(s: &str) -> Result<Self, String> {
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

        emails.push(Email {
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
        });
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

    Ok(Fixture {
        name: raw.name,
        state: raw.state.unwrap_or_else(|| "fixture-state".to_string()),
        account: Account {
            id: raw.account.id,
            name: raw.account.name,
        },
        mailboxes,
        emails,
        oauth,
    })
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid RFC3339 timestamp {s:?}: {e}"))
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
