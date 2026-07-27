# ratatoskr EWS + Autodiscover surface

What ratatoskr's Exchange Web Services (EWS) client and the SOAP
Autodiscover flow expect on the wire, and what the mock serves. Drives
A5b (public folders + EWS-streaming push).

Unlike the other protocol surface docs, this one is authored from the
published EWS / Autodiscover SOAP schemas plus the ratatoskr gap report
(GetUserSettings, FindFolder / FindItem, GetStreamingEvents) rather
than from a citation-backed read of the client - there was no prior
notes doc. Tighten it against `<ratatoskr>` when the real EWS client
integration lands.

## Listener

Own listener on `--ews-port`; sentinel line `EWS <port>`. POST
endpoints:

- `/autodiscover/autodiscover.svc` - SOAP Autodiscover
  (`GetUserSettings`).
- `/autodiscover/autodiscover.xml` - POX Autodiscover (delegate /
  shared-mailbox discovery).
- `/EWS/Exchange.asmx` - EWS operations.

Both Autodiscover paths are also registered under the capitalised
spelling real Outlook-family clients use
(`/Autodiscover/Autodiscover.{svc,xml}`); axum matches paths
case-sensitively.

### Also mounted on the Graph listener

The same router is merged into the Graph listener's router
(`main.rs`), so every path above answers identically on the Graph
port. The reason is harness plumbing, not protocol modelling: brokkr
injects a fixed set of `RATATOSKR_TEST_*_ENDPOINT` variables into the
process under test and there is no EWS slot among them, so a harness
run cannot reach the dedicated EWS listener at all. Co-mounting makes
the surface reachable without changing the harness contract. The
dedicated listener keeps working unchanged.

The two mounts are separate routers over the same `SharedHandles`,
each with its own `base_url`, so the EWS / Autodiscover URLs a client
gets back point at the listener its request arrived on. The merged
routes sit outside the Graph router's bearer-enforcement and
request-log middleware (they are added after those layers), which
matches EWS being accept-all; EWS logs its own entries under the
`ews` protocol tag either way.

SOAP over plain HTTP; every response is `text/xml; charset=utf-8`
wrapped in a `<s:Envelope><s:Body>...</s:Body></s:Envelope>`. XML is
hand-rolled (`src/ews/xml.rs`) in the CalDAV small-well-known-bodies
style. Auth is accept-all (no bearer enforcement); a streaming
`Subscribe` resolves its account from the bearer if one is present,
else the primary account.

## Autodiscover (GetUserSettings)

`POST /autodiscover/autodiscover.svc` with a
`GetUserSettingsRequestMessage` returns a `GetUserSettingsResponse`
carrying `ErrorCode = NoError` and one `UserSetting`:
`ExternalEwsUrl = <this listener>/EWS/Exchange.asmx`. The URL is built
from the EWS listener's own bound base URL (`main.rs`), so a client
that autodiscovers binds back to this process. Other settings
(CasVersion, ExternalMailboxServer, ...) are omitted - v0 clients only
need the EWS URL.

## Autodiscover POX (delegate / shared-mailbox discovery)

`POST /autodiscover/autodiscover.xml` with a POX `<Autodiscover>`
request. This is a different channel from the SOAP endpoint above: it
is how a client performing delegate / shared-mailbox discovery learns
which *other* mailboxes it should open.

The `<EMailAddress>` in the request selects the requesting account
(matched case-insensitively against the declared `[[account]]` names;
an unknown address falls back to the primary). The response carries:

- `<User>` - DisplayName / EMailAddress / LegacyDN for that account.
- `<Account><Protocol>` - an `EXCH` protocol block with `EwsUrl` /
  `EmwsUrl` built from this mount's base URL, so a POX-discovering
  client binds back to this process.
- `<AlternativeMailboxes>` - one `<AlternativeMailbox>` per declared
  account *other* than the requesting one, each with
  `Type = Delegate`, DisplayName, SmtpAddress, OwnerSmtpAddress,
  LegacyDN, Server. This is the shared-mailbox projection: declare a
  second `[[account]]` and it shows up here.

Deviation from the published POX schema, deliberate: real POX emits
repeated bare `<AlternativeMailbox>` children of `<Account>` with no
container element. We wrap them in an `<AlternativeMailboxes>`
container (the name the SOAP `AlternateMailboxes` setting uses) so a
consumer can key off either the container or the individual entries;
a parser walking by local name sees the entries unchanged. Tighten
this if the real client turns out to require the bare form.

A body that is not an Autodiscover request gets the POX in-band error
document (HTTP 400, `<ErrorCode>600</ErrorCode>`), not a SOAP fault -
POX is not SOAP and is not envelope-wrapped.

## Public folders (read-only)

Backed by the fixture `[[public_folder]]` / `[[public_item]]` tables
(org-wide, no account scope). Dispatch on `/EWS/Exchange.asmx` is by
the local name of the first element inside `soap:Body`.

- `FindFolder` - `Traversal="Deep"` returns the whole tree;
  `Shallow` (default) returns the children of the named parent
  (`DistinguishedFolderId Id="publicfoldersroot"` or an absent parent
  -> top-level folders; an explicit `FolderId Id="..."` -> that
  folder's children). Each `t:Folder` carries `FolderId` (+ opaque
  `ChangeKey`), `ParentFolderId` (`publicfoldersroot` for a top-level
  folder), `FolderClass`, `DisplayName`, `TotalCount`,
  `ChildFolderCount`, and `EffectiveRights`. Element order follows the
  EWS `t:FolderType` sequence.
- `FindItem` - items in the `ParentFolderIds/FolderId`. `BaseShape`
  `IdOnly` emits just the `ItemId`; anything else emits Subject,
  DateTimeReceived, From. Unknown folder -> `ErrorFolderNotFound`.
- `GetItem` - hydrate item bodies by `ItemIds/ItemId`. Each requested
  id gets its own `GetItemResponseMessage`; an unknown id degrades that
  one message to `ErrorItemNotFound` (per-item, not batch-wide). The
  `t:Message` carries ItemId, ParentFolderId, Subject,
  DateTimeReceived, `Body`, `Attachments` (metadata only, omitted when
  there are none), `HasAttachments`, From, and ToRecipients. The body
  is `BodyType="HTML"` when the fixture item stages `body_html`, else
  `BodyType="Text"` from `body_text`.
- `GetAttachment` - attachment bytes by `AttachmentIds/AttachmentId`,
  base64 in `<t:Content>`. This is the other half of the hydration
  split: `GetItem` hands out metadata, `GetAttachment` hands out
  content. EWS addresses an attachment by id alone with no item
  context, so the loader enforces org-wide uniqueness of public
  attachment blob ids. An unknown id degrades that one message to
  `ErrorInvalidAttachmentId`.

### Folder class

`[[public_folder]] folder_class` (default `IPF.Note`) is emitted as
`t:FolderClass`. Stage an `IPF.Appointment` (or `IPF.Contact`,
`IPF.Task`, ...) folder alongside a mail one when a consumer has to
prove it keeps non-mail public folders out of its mail path.

### Effective rights

`[[public_folder]] effective_rights` drives the `t:EffectiveRights`
block (CreateAssociated / CreateContents / CreateHierarchy / Delete /
Modify / Read / ViewPrivateItems, in that schema order). Omitted, a
folder is read-only (`Read` alone) - what an org-wide public folder
grants an ordinary user. `fixtures/shared-rights.toml` stages a
read-only and a writable public folder in the same fixture.

Note the rights are *reported*, not enforced: there is still no EWS
write surface, so nothing can test-drive the enforcement side. A
consumer asserts on what it reads.

`ChangeKey` derives from the fixture primary state token (opaque; moves
in lock-step on a mutation). There is no EWS write surface in v0.

## Streaming notifications (wired to PushHub)

Streaming-subscription lifecycle, wired through the shared
`crate::push::PushHub` like the JMAP WebSocket / Gmail Pub/Sub / Graph
webhook surfaces:

- `Subscribe` (StreamingSubscriptionRequest) - registers a
  subscription scoped to the bearer-resolved account; returns a minted
  `SubscriptionId`.
- `GetStreamingEvents` - drains and returns the notifications queued
  for the subscription since the last poll. A fixture state advance
  (`POST /test/fixture/step` -> `PushHub::emit_state_advance`) enqueues
  one `NewMailEvent` (Watermark + TimeStamp + a `ParentFolderId
  Id="inbox"`) per subscription bound to the touched account. Draining
  is one-shot. Known subscription, no pending events -> `OK`
  heartbeat with an empty `Notifications`. Unknown subscription ->
  `ConnectionStatus = Closed` + `ErrorInvalidSubscription`.
- `Unsubscribe` - drops the subscription (and its queue).

This is a poll-drain model, not a true held-open long-poll: it keeps
the surface deterministic (step, then poll) and testable via
`oneshot`. NewMail events omit a fabricated `ItemId` - the Watermark +
TimeStamp are enough to prove the push fired; add a real item id if a
client reacts to the event by `GetItem`-ing it.

## Out of scope for v0

- EWS write operations (CreateItem / UpdateItem / DeleteItem /
  CreateFolder / ...).
- Pull / push subscriptions (only streaming is modelled).
- SyncFolderHierarchy / SyncFolderItems delta.
- Held-open long-poll `GetStreamingEvents` (poll-drain instead).
- POX Autodiscover redirect flows (`RedirectAddr` / `RedirectUrl`),
  the `AcceptableResponseSchema` negotiation, and the SRV / CNAME
  discovery cascade that precedes the POX POST. The POX endpoint
  answers the direct POST only.
- Enforcement of `EffectiveRights` (they are reported, not enforced -
  there is no write surface to enforce them against).
- NTLM / Basic / OAuth enforcement (accept-all).
