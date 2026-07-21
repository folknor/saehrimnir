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

Own listener on `--ews-port`; sentinel line `EWS <port>`. Two POST
endpoints:

- `/autodiscover/autodiscover.svc` - SOAP Autodiscover.
- `/EWS/Exchange.asmx` - EWS operations.

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

## Public folders (read-only)

Backed by the fixture `[[public_folder]]` / `[[public_item]]` tables
(org-wide, no account scope). Dispatch on `/EWS/Exchange.asmx` is by
the local name of the first element inside `soap:Body`.

- `FindFolder` - `Traversal="Deep"` returns the whole tree;
  `Shallow` (default) returns the children of the named parent
  (`DistinguishedFolderId Id="publicfoldersroot"` or an absent parent
  -> top-level folders; an explicit `FolderId Id="..."` -> that
  folder's children). Each `t:Folder` carries `FolderId` (+ opaque
  `ChangeKey`), `DisplayName`, `TotalCount`, `ChildFolderCount`.
- `FindItem` - items in the `ParentFolderIds/FolderId`. `BaseShape`
  `IdOnly` emits just the `ItemId`; anything else emits Subject,
  DateTimeReceived, From. Unknown folder -> `ErrorFolderNotFound`.
- `GetItem` - hydrate item bodies by `ItemIds/ItemId`. Each requested
  id gets its own `GetItemResponseMessage`; an unknown id degrades that
  one message to `ErrorItemNotFound` (per-item, not batch-wide). The
  `t:Message` carries ItemId, Subject, DateTimeReceived, a
  `Body BodyType="Text"`, From, and ToRecipients.

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
- Autodiscover POX (`/autodiscover/autodiscover.xml`); only the SOAP
  `GetUserSettings` form is served.
- NTLM / Basic / OAuth enforcement (accept-all).
