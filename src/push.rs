//! Provider push surfaces (JMAP WebSocket / Gmail Pub/Sub / Graph
//! webhooks).
//!
//! Sæhrimnir's read + delta surfaces are pull-only: a client polls
//! `Email/changes`, `history.list`, `messages/delta`. Real providers
//! also *push* - they tell the client "your state moved, come fetch"
//! over a side channel. [`PushHub`] is the side-channel hub. It is
//! cloned into every HTTP `AppState` via [`crate::shared::SharedHandles`]
//! so the client-facing subscribe endpoints (JMAP `/jmap/ws`, Gmail
//! `users.watch` / `users.stop`, Graph `POST /subscriptions`) and the
//! emission trigger all share one registry.
//!
//! ## Trigger
//!
//! Emission is driven by the *existing* test-admin state-mutation path:
//! when `POST /test/fixture/step` applies a change-script step and
//! `Fixture::record_transition` advances an account's state, the step
//! handler calls [`PushHub::emit_state_advance`] once per touched
//! account (resolved via `Fixture::accounts_touched`). A mutation on
//! account B never pushes account A.
//!
//! ## Wire contracts (real, so a real client consumes them unchanged)
//!
//! - **JMAP** (RFC 8620 §7.1 `StateChange` over the RFC 8887 WebSocket
//!   channel): a `{ "@type": "StateChange", "changed": { <accountId>:
//!   { "Email": <state>, "Mailbox": <state>, ... } } }` text frame to
//!   every WebSocket registered for that account.
//! - **Gmail** (Cloud Pub/Sub push): a Pub/Sub push envelope
//!   `{ "message": { "data": base64({ emailAddress, historyId }), ... },
//!   "subscription": ... }` published onto the in-memory Pub/Sub sink
//!   (and POSTed to a registered push endpoint when one is set). Fires
//!   only for an account with an active `users.watch`.
//! - **Graph** (change notifications): a `{ "value": [ { subscriptionId,
//!   changeType, resource, resourceData, clientState, ... } ] }` body
//!   POSTed to the subscription's stored loopback `notificationUrl`,
//!   carrying the stored `clientState`. Fires for every subscription
//!   bound to that account.
//!
//! ## Observability
//!
//! Every emitted JMAP frame and Graph notification is also appended to
//! an in-memory log, and every Pub/Sub message to the sink, so harness
//! scripts (and the integration tests) can assert "push fired" over the
//! `/test/push/*` + `/test/gmail/pubsub/*` admin routes without holding
//! a live socket open. Live transport (WebSocket send, loopback POST)
//! happens alongside the log write for a real client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Deterministic stand-in for Pub/Sub's wall-clock `publishTime`. Real
/// Pub/Sub stamps the publish instant; the mock pins it so rendered
/// envelopes stay byte-stable across runs. Tests assert on the decoded
/// `data` payload, never on this field.
const PUBLISH_TIME: &str = "2026-01-01T00:00:00Z";

/// Per-account snapshot the trigger hands [`PushHub::emit_state_advance`].
/// Built by the step handler while it still holds the fixture guard, so
/// the hub never needs to re-lock the fixture.
#[derive(Debug, Clone)]
pub struct AccountPush {
    /// The account whose state advanced.
    pub account_id: String,
    /// The account's email address (Gmail Pub/Sub `emailAddress`).
    pub email_address: String,
    /// The account's new state token (`Fixture::state_for`).
    pub state: String,
    /// The account's new Gmail `historyId` (`Fixture::gmail_history_id`).
    pub history_id: u64,
    /// Whether the account owns calendars (gates `CalendarEvent` in the
    /// JMAP `StateChange` map).
    pub has_calendars: bool,
    /// Whether the account owns contact folders (gates `ContactCard`).
    pub has_contacts: bool,
    /// Dominant change kind for the Graph notification `changeType`
    /// (`created` / `updated` / `deleted`).
    pub change_type: String,
    /// A representative changed message id for the Graph notification's
    /// `resourceData`, when the mutation touched a message.
    pub resource_id: Option<String>,
}

/// A live JMAP WebSocket push registration.
struct JmapListener {
    account_id: String,
    tx: mpsc::UnboundedSender<String>,
}

/// A live IMAP `IDLE` registration. The IMAP connection parks in
/// `IDLE` on a selected mailbox and waits for a wake; on each wake it
/// re-reads the fixture and emits the unsolicited untagged responses
/// (`* n EXISTS` / `* n EXPUNGE` / `* n RECENT`) for whatever moved.
/// The wake carries no payload - the idling connection already knows
/// its selected mailbox and recomputes the diff itself, which keeps
/// this hub free of any IMAP wire knowledge and robust to coalesced
/// wakes (two mutations before the connection drains the channel still
/// produce one correct diff).
struct ImapIdleListener {
    account_id: String,
    tx: mpsc::UnboundedSender<()>,
}

/// A Gmail `users.watch` registration. Stored per account.
#[derive(Debug, Clone)]
pub struct GmailWatch {
    pub account_id: String,
    pub topic_name: String,
    pub label_ids: Vec<String>,
}

/// An EWS `Subscribe` (StreamingSubscription) registration. Bound to
/// the mailbox account it watches; a `GetStreamingEvents` poll for the
/// id drains the account's queued notifications.
#[derive(Debug, Clone)]
pub struct EwsSubscription {
    pub id: String,
    pub account_id: String,
}

/// A Graph `POST /subscriptions` registration.
#[derive(Debug, Clone)]
pub struct GraphSubscription {
    pub id: String,
    pub account_id: String,
    pub resource: String,
    pub change_type: String,
    pub client_state: Option<String>,
    pub notification_url: String,
    pub expiration: String,
}

struct Inner {
    /// Monotonic id source for minted ids (Pub/Sub messageId, Graph
    /// subscription id). Process-scoped; deterministic per call order.
    seq: AtomicU64,
    jmap_ws: Mutex<Vec<JmapListener>>,
    imap_idle: Mutex<Vec<ImapIdleListener>>,
    jmap_log: Mutex<Vec<Value>>,
    gmail_watches: Mutex<HashMap<String, GmailWatch>>,
    /// account_id -> loopback URL a Pub/Sub push is delivered to (real
    /// Pub/Sub delivers to the subscription's push endpoint, configured
    /// out-of-band; the harness registers it here).
    gmail_push_endpoints: Mutex<HashMap<String, String>>,
    pubsub: Mutex<Vec<Value>>,
    graph_subs: Mutex<HashMap<String, GraphSubscription>>,
    graph_log: Mutex<Vec<Value>>,
    /// EWS streaming subscriptions by id.
    ews_subs: Mutex<HashMap<String, EwsSubscription>>,
    /// Queued EWS notification events by subscription id. A state
    /// advance enqueues one event per matching subscription;
    /// `GetStreamingEvents` drains the queue for its id. Each event is
    /// `{ "watermark": <string>, "timestamp": <rfc3339 string> }`.
    ews_events: Mutex<HashMap<String, Vec<Value>>>,
}

/// Cheap-to-clone handle to the shared push registry. See module docs.
#[derive(Clone)]
pub struct PushHub {
    inner: Arc<Inner>,
}

impl Default for PushHub {
    fn default() -> Self {
        Self::new()
    }
}

impl PushHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(1),
                jmap_ws: Mutex::new(Vec::new()),
                imap_idle: Mutex::new(Vec::new()),
                jmap_log: Mutex::new(Vec::new()),
                gmail_watches: Mutex::new(HashMap::new()),
                gmail_push_endpoints: Mutex::new(HashMap::new()),
                pubsub: Mutex::new(Vec::new()),
                graph_subs: Mutex::new(HashMap::new()),
                graph_log: Mutex::new(Vec::new()),
                ews_subs: Mutex::new(HashMap::new()),
                ews_events: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Next monotonic id (for minted subscription ids etc.).
    pub fn next_seq(&self) -> u64 {
        self.inner.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ── JMAP WebSocket registry ─────────────────────────────────────

    /// Register a JMAP push listener for `account_id`. Returns the
    /// receiver end; the caller forwards each frame onto its WebSocket.
    /// A closed receiver is pruned lazily on the next emit.
    pub fn register_jmap_ws(&self, account_id: String) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner
            .jmap_ws
            .lock()
            .expect("jmap_ws lock poisoned")
            .push(JmapListener { account_id, tx });
        rx
    }

    /// Snapshot of every JMAP `StateChange` frame emitted so far.
    pub fn jmap_log(&self) -> Vec<Value> {
        self.inner
            .jmap_log
            .lock()
            .expect("jmap_log lock poisoned")
            .clone()
    }

    // ── IMAP IDLE registry ──────────────────────────────────────────

    /// Register an IMAP `IDLE` waiter for `account_id`. Returns the
    /// receiver end; the idling connection awaits it and emits the
    /// unsolicited untagged responses on each wake. A closed receiver
    /// (the connection left `IDLE` / dropped) is pruned lazily on the
    /// next emit.
    pub fn register_imap_idle(&self, account_id: String) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner
            .imap_idle
            .lock()
            .expect("imap_idle lock poisoned")
            .push(ImapIdleListener { account_id, tx });
        rx
    }

    // ── Gmail watch + Pub/Sub registry ──────────────────────────────

    /// Record a `users.watch`. Replaces any prior watch on the account.
    pub fn gmail_watch_set(&self, watch: GmailWatch) {
        self.inner
            .gmail_watches
            .lock()
            .expect("gmail_watches lock poisoned")
            .insert(watch.account_id.clone(), watch);
    }

    /// Clear a `users.stop`. Returns whether a watch was present.
    pub fn gmail_watch_clear(&self, account_id: &str) -> bool {
        self.inner
            .gmail_watches
            .lock()
            .expect("gmail_watches lock poisoned")
            .remove(account_id)
            .is_some()
    }

    pub fn gmail_watch_get(&self, account_id: &str) -> Option<GmailWatch> {
        self.inner
            .gmail_watches
            .lock()
            .expect("gmail_watches lock poisoned")
            .get(account_id)
            .cloned()
    }

    /// Register the loopback URL a Pub/Sub push for `account_id` is
    /// delivered to (mirrors a real Pub/Sub subscription's push config).
    pub fn gmail_set_push_endpoint(&self, account_id: String, url: String) {
        self.inner
            .gmail_push_endpoints
            .lock()
            .expect("gmail_push_endpoints lock poisoned")
            .insert(account_id, url);
    }

    /// Snapshot of every Pub/Sub envelope published so far.
    pub fn pubsub_messages(&self) -> Vec<Value> {
        self.inner
            .pubsub
            .lock()
            .expect("pubsub lock poisoned")
            .clone()
    }

    pub fn pubsub_clear(&self) {
        self.inner
            .pubsub
            .lock()
            .expect("pubsub lock poisoned")
            .clear();
    }

    /// Publish a `{ emailAddress, historyId }` Gmail notification onto
    /// the mock Pub/Sub source. Used by the test-admin manual-publish
    /// route; the state-mutation trigger goes through
    /// [`Self::emit_state_advance`]. Returns the envelope it published.
    pub fn publish_gmail(&self, email: &str, history_id: u64, account_id: &str) -> Value {
        let subscription = self.subscription_for(account_id);
        let env = self.pubsub_envelope(email, history_id, &subscription);
        self.publish_pubsub(env.clone(), account_id);
        env
    }

    // ── Graph subscription registry ─────────────────────────────────

    pub fn graph_create_subscription(&self, sub: GraphSubscription) {
        self.inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned")
            .insert(sub.id.clone(), sub);
    }

    pub fn graph_get_subscription(&self, id: &str) -> Option<GraphSubscription> {
        self.inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned")
            .get(id)
            .cloned()
    }

    /// Renew a subscription's expiration. Returns the updated record, or
    /// `None` if no such subscription is registered.
    pub fn graph_renew_subscription(
        &self,
        id: &str,
        expiration: String,
    ) -> Option<GraphSubscription> {
        let mut subs = self
            .inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned");
        let sub = subs.get_mut(id)?;
        sub.expiration = expiration;
        Some(sub.clone())
    }

    /// Delete a subscription. Returns whether one was present.
    pub fn graph_delete_subscription(&self, id: &str) -> bool {
        self.inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned")
            .remove(id)
            .is_some()
    }

    /// Snapshot of every Graph notification emitted so far. Each entry
    /// is `{ "notification_url": <url>, "body": <notification> }`.
    pub fn graph_log(&self) -> Vec<Value> {
        self.inner
            .graph_log
            .lock()
            .expect("graph_log lock poisoned")
            .clone()
    }

    // ── EWS streaming-subscription registry ─────────────────────────

    pub fn ews_create_subscription(&self, sub: EwsSubscription) {
        self.inner
            .ews_subs
            .lock()
            .expect("ews_subs lock poisoned")
            .insert(sub.id.clone(), sub);
    }

    pub fn ews_get_subscription(&self, id: &str) -> Option<EwsSubscription> {
        self.inner
            .ews_subs
            .lock()
            .expect("ews_subs lock poisoned")
            .get(id)
            .cloned()
    }

    /// Delete a streaming subscription and discard any queued events.
    /// Returns whether one was present.
    pub fn ews_delete_subscription(&self, id: &str) -> bool {
        self.inner
            .ews_events
            .lock()
            .expect("ews_events lock poisoned")
            .remove(id);
        self.inner
            .ews_subs
            .lock()
            .expect("ews_subs lock poisoned")
            .remove(id)
            .is_some()
    }

    /// Drain (and clear) the queued notification events for a
    /// subscription. `GetStreamingEvents` returns these to the client.
    /// An unknown / empty subscription yields an empty batch.
    pub fn ews_drain_events(&self, id: &str) -> Vec<Value> {
        self.inner
            .ews_events
            .lock()
            .expect("ews_events lock poisoned")
            .get_mut(id)
            .map(std::mem::take)
            .unwrap_or_default()
    }

    // ── Reset ───────────────────────────────────────────────────────

    /// Clear volatile push state (logs, sinks, watches, subscriptions,
    /// endpoints). Live WebSocket registrations are intentionally kept -
    /// a `POST /test/fixture/reset` rewinds the fixture image but must
    /// not yank a connected client's socket. Mirrors the rest of the
    /// reset surface (request log, token store, latency).
    pub fn clear(&self) {
        self.inner
            .jmap_log
            .lock()
            .expect("jmap_log lock poisoned")
            .clear();
        self.inner
            .gmail_watches
            .lock()
            .expect("gmail_watches lock poisoned")
            .clear();
        self.inner
            .gmail_push_endpoints
            .lock()
            .expect("gmail_push_endpoints lock poisoned")
            .clear();
        self.inner
            .pubsub
            .lock()
            .expect("pubsub lock poisoned")
            .clear();
        self.inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned")
            .clear();
        self.inner
            .graph_log
            .lock()
            .expect("graph_log lock poisoned")
            .clear();
        self.inner
            .ews_subs
            .lock()
            .expect("ews_subs lock poisoned")
            .clear();
        self.inner
            .ews_events
            .lock()
            .expect("ews_events lock poisoned")
            .clear();
    }

    // ── Emission ────────────────────────────────────────────────────

    /// Fire every push surface for each account whose state advanced.
    /// Called by the test-admin step handler after `record_transition`.
    /// Synchronous: log/sink writes happen inline, live transport
    /// (WebSocket send is non-blocking; loopback POSTs are spawned) does
    /// not await under any lock.
    pub fn emit_state_advance(&self, pushes: &[AccountPush]) {
        for p in pushes {
            self.emit_jmap(p);
            self.emit_imap(p);
            self.emit_gmail(p);
            self.emit_graph(p);
            self.emit_ews(p);
        }
    }

    /// Enqueue one EWS streaming notification per subscription bound to
    /// the account whose state advanced. `GetStreamingEvents` drains
    /// the queue. The watermark is a monotonic id; the timestamp mirrors
    /// the push snapshot's history id (a stand-in wall clock).
    fn emit_ews(&self, p: &AccountPush) {
        let sub_ids: Vec<String> = self
            .inner
            .ews_subs
            .lock()
            .expect("ews_subs lock poisoned")
            .values()
            .filter(|s| s.account_id == p.account_id)
            .map(|s| s.id.clone())
            .collect();
        if sub_ids.is_empty() {
            return;
        }
        let watermark = format!("W{}", self.next_seq());
        let mut events = self
            .inner
            .ews_events
            .lock()
            .expect("ews_events lock poisoned");
        for id in sub_ids {
            events.entry(id).or_default().push(serde_json::json!({
                "watermark": watermark,
                "timestamp": p.history_id.to_string(),
            }));
        }
    }

    /// Wake every IMAP `IDLE` waiter registered for this account, so an
    /// idling connection selected on a mailbox in the account recomputes
    /// and emits its unsolicited untagged responses. Prunes any waiter
    /// whose receiver was dropped (the connection left `IDLE`).
    fn emit_imap(&self, p: &AccountPush) {
        let mut waiters = self
            .inner
            .imap_idle
            .lock()
            .expect("imap_idle lock poisoned");
        waiters.retain(|w| {
            if w.account_id == p.account_id {
                w.tx.send(()).is_ok()
            } else {
                true
            }
        });
    }

    fn emit_jmap(&self, p: &AccountPush) {
        let frame = jmap_state_change(p);
        let text = serde_json::to_string(&frame).unwrap_or_default();
        {
            let mut listeners = self.inner.jmap_ws.lock().expect("jmap_ws lock poisoned");
            // Send to each live listener for this account; prune any
            // whose receiver was dropped (the WebSocket closed).
            listeners.retain(|l| {
                if l.account_id == p.account_id {
                    l.tx.send(text.clone()).is_ok()
                } else {
                    true
                }
            });
        }
        self.inner
            .jmap_log
            .lock()
            .expect("jmap_log lock poisoned")
            .push(frame);
    }

    fn emit_gmail(&self, p: &AccountPush) {
        let Some(watch) = self.gmail_watch_get(&p.account_id) else {
            return;
        };
        let subscription = watch.topic_name.replace("/topics/", "/subscriptions/");
        let env = self.pubsub_envelope(&p.email_address, p.history_id, &subscription);
        self.publish_pubsub(env, &p.account_id);
    }

    fn emit_graph(&self, p: &AccountPush) {
        let subs: Vec<GraphSubscription> = self
            .inner
            .graph_subs
            .lock()
            .expect("graph_subs lock poisoned")
            .values()
            .filter(|s| s.account_id == p.account_id)
            .cloned()
            .collect();
        for sub in subs {
            let note = graph_notification(&sub, p);
            self.inner
                .graph_log
                .lock()
                .expect("graph_log lock poisoned")
                .push(json!({
                    "notification_url": sub.notification_url,
                    "body": note.clone(),
                }));
            let url = sub.notification_url.clone();
            let body = serde_json::to_vec(&note).unwrap_or_default();
            tokio::spawn(async move {
                if let Err(e) = deliver_post(&url, "application/json", body).await {
                    eprintln!("saehrimnir: graph push delivery to {url} failed: {e}");
                }
            });
        }
    }

    // ── Internals ───────────────────────────────────────────────────

    fn subscription_for(&self, account_id: &str) -> String {
        self.gmail_watch_get(account_id)
            .map(|w| w.topic_name.replace("/topics/", "/subscriptions/"))
            .unwrap_or_else(|| "projects/saehrimnir/subscriptions/mock".to_string())
    }

    fn pubsub_envelope(&self, email: &str, history_id: u64, subscription: &str) -> Value {
        let data = json!({ "emailAddress": email, "historyId": history_id });
        let encoded = base64_standard(serde_json::to_string(&data).unwrap_or_default().as_bytes());
        let seq = self.next_seq();
        json!({
            "message": {
                "data": encoded,
                "messageId": format!("mock-msg-{seq}"),
                "publishTime": PUBLISH_TIME,
                "attributes": {},
            },
            "subscription": subscription,
        })
    }

    fn publish_pubsub(&self, env: Value, account_id: &str) {
        let endpoint = self
            .inner
            .gmail_push_endpoints
            .lock()
            .expect("gmail_push_endpoints lock poisoned")
            .get(account_id)
            .cloned();
        if let Some(url) = endpoint {
            let body = serde_json::to_vec(&env).unwrap_or_default();
            tokio::spawn(async move {
                if let Err(e) = deliver_post(&url, "application/json", body).await {
                    eprintln!("saehrimnir: gmail pubsub delivery to {url} failed: {e}");
                }
            });
        }
        self.inner
            .pubsub
            .lock()
            .expect("pubsub lock poisoned")
            .push(env);
    }
}

/// Build the RFC 8620 §7.1 `StateChange` frame for one account. Every
/// datatype that carries state reports the same per-account token (the
/// mock has no per-datatype state), gated on the account owning the
/// resource family.
fn jmap_state_change(p: &AccountPush) -> Value {
    let mut types = serde_json::Map::new();
    types.insert("Email".to_string(), json!(p.state));
    types.insert("Mailbox".to_string(), json!(p.state));
    types.insert("Thread".to_string(), json!(p.state));
    types.insert("EmailDelivery".to_string(), json!(p.state));
    if p.has_calendars {
        types.insert("CalendarEvent".to_string(), json!(p.state));
    }
    if p.has_contacts {
        types.insert("ContactCard".to_string(), json!(p.state));
    }
    let mut changed = serde_json::Map::new();
    changed.insert(p.account_id.clone(), Value::Object(types));
    json!({
        "@type": "StateChange",
        "changed": Value::Object(changed),
    })
}

/// Build a Graph change-notification body for one subscription.
fn graph_notification(sub: &GraphSubscription, p: &AccountPush) -> Value {
    let mut resource_data = serde_json::Map::new();
    resource_data.insert("@odata.type".to_string(), json!("#Microsoft.Graph.Message"));
    if let Some(id) = &p.resource_id {
        resource_data.insert("id".to_string(), json!(id));
        resource_data.insert(
            "@odata.id".to_string(),
            json!(format!("{}/{}", sub.resource, id)),
        );
    }
    let mut note = serde_json::Map::new();
    note.insert("subscriptionId".to_string(), json!(sub.id));
    note.insert(
        "subscriptionExpirationDateTime".to_string(),
        json!(sub.expiration),
    );
    note.insert("changeType".to_string(), json!(p.change_type));
    note.insert("resource".to_string(), json!(sub.resource));
    note.insert("resourceData".to_string(), Value::Object(resource_data));
    if let Some(cs) = &sub.client_state {
        note.insert("clientState".to_string(), json!(cs));
    }
    note.insert("tenantId".to_string(), json!("mock-tenant"));
    json!({ "value": [Value::Object(note)] })
}

/// Minimal fire-and-read loopback HTTP/1.1 POST. Targets only plain
/// `http://` loopback endpoints (the harness's notification receivers);
/// the response is read to EOF and discarded. Hand-rolled to avoid
/// pulling a full HTTP client into the binary - the project hand-rolls
/// its other small wire surfaces (base64url, CalDAV XML) for the same
/// reason.
async fn deliver_post(url: &str, content_type: &str, body: Vec<u8>) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url scheme (want http://): {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let mut stream = tokio::net::TcpStream::connect(&host_port)
        .await
        .map_err(|e| format!("connect {host_port}: {e}"))?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;
    let mut sink = Vec::new();
    stream
        .read_to_end(&mut sink)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Standard (RFC 4648 §4) base64 with padding - the encoding Cloud
/// Pub/Sub uses for the `message.data` field. Distinct from the Gmail
/// REST surface's base64*url* (used for raw message bytes); a Pub/Sub
/// consumer decodes this with a standard-alphabet decoder.
fn base64_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_standard_matches_known_vectors() {
        assert_eq!(base64_standard(b""), "");
        assert_eq!(base64_standard(b"f"), "Zg==");
        assert_eq!(base64_standard(b"fo"), "Zm8=");
        assert_eq!(base64_standard(b"foo"), "Zm9v");
        assert_eq!(base64_standard(b"foob"), "Zm9vYg==");
        assert_eq!(base64_standard(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_standard(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn jmap_frame_gates_calendar_and_contacts() {
        let base = AccountPush {
            account_id: "account-1".to_string(),
            email_address: "a@example.com".to_string(),
            state: "s.3".to_string(),
            history_id: 4,
            has_calendars: false,
            has_contacts: false,
            change_type: "updated".to_string(),
            resource_id: None,
        };
        let frame = jmap_state_change(&base);
        let changed = &frame["changed"]["account-1"];
        assert_eq!(changed["Email"], "s.3");
        assert_eq!(changed["Mailbox"], "s.3");
        assert!(changed.get("CalendarEvent").is_none());
        assert!(changed.get("ContactCard").is_none());

        let with = AccountPush {
            has_calendars: true,
            has_contacts: true,
            ..base
        };
        let frame = jmap_state_change(&with);
        let changed = &frame["changed"]["account-1"];
        assert_eq!(changed["CalendarEvent"], "s.3");
        assert_eq!(changed["ContactCard"], "s.3");
    }
}
