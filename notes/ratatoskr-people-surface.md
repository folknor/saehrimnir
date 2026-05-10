# Ratatoskr's People API client surface

What the v0 mock has to satisfy. Distilled from
`<ratatoskr>/crates/gmail/src/contacts/` on 2026-05-10.
Source-of-truth lives there; this file is a cheat sheet.

## Connection

- Real Google People API host: `https://people.googleapis.com/v1`.
  Different from Gmail's `https://www.googleapis.com/gmail/v1/users/me`.
  Source: `crates/gmail/src/contacts/mod.rs:112`.
- ratatoskr's HTTP client uses `GmailClient::get_absolute(&url, db)`
  with the absolute URL; bearer auth re-uses the Gmail OAuth token.
- ratatoskr does NOT yet have a `RATATOSKR_TEST_PEOPLE_ENDPOINT`
  override. Until that lands, sæhrimnir's People listener is
  reachable for unit tests via `oneshot` against the router but
  unreachable from a live ratatoskr binary; the hardcoded
  `PEOPLE_API_BASE` in ratatoskr blocks us. Adding the override
  follows the same shape as `RATATOSKR_TEST_GMAIL_ENDPOINT` in
  `crates/gmail/src/client.rs:73-82`.

## Endpoints invoked

In order:

1. `GET /v1/people/me/connections` - first call, no `syncToken`.
   Query params:
   - `personFields=<comma-separated>` (the "fields ratatoskr
     reads back" - see Person shape below).
   - `pageSize=1000` (default), or whatever the client picks.
   - `requestSyncToken=true` (always set).
   - `pageToken=...` (subsequent pages).
   Source:
   `crates/gmail/src/contacts/google_contacts.rs:64-66, 138-140`.

2. `GET /v1/people/me/connections?syncToken=<previous>` - delta
   sync. Errors containing `"410"` or `"GONE"` or the substring
   `"syncToken"` trigger a full re-sync via the no-token call.
   Source: `:37`.

3. `GET /v1/otherContacts` - same shape as connections, but
   wrapping the response in `otherContacts[]` instead of
   `connections[]` and the count in `totalSize` instead of
   `totalPeople` / `totalItems`.

## Response shapes

`PeopleConnectionsResponse`
(`crates/gmail/src/contacts/mod.rs:26-34`):

```
{
  "connections": [Person, ...],
  "nextPageToken": <string?>,
  "nextSyncToken": <string?>,
  "totalPeople": <int?>,
  "totalItems": <int?>
}
```

`OtherContactsResponse` (`:36-43`):

```
{
  "otherContacts": [Person, ...],
  "nextPageToken": <string?>,
  "nextSyncToken": <string?>,
  "totalSize": <int?>
}
```

`Person` (`:45-56`): every field is `Option<...>`, and ratatoskr
tolerates omission everywhere. The fields it actually reads:

- `resourceName` (e.g. `people/c123`) - keyed in DB.
- `etag` - opaque pass-through.
- `metadata.deleted: bool` - tombstones.
- `metadata.sources[]: {type, id}` - currently ignored, just
  present.
- `names[].displayName` / `givenName` / `familyName` - first entry
  only.
- `emailAddresses[].value` and `.type` - first valid lower-cased
  email becomes the keying field for the
  `google_contact_<account>_<email>` mapping.
- `phoneNumbers[]`, `organizations[]`, `photos[]` - read but
  optional; ratatoskr stores the first photo URL as the avatar.

## Sync-token contract

Real People API errors with HTTP 410 (`reason: "expiredSyncToken"`)
when the saved token is no longer valid. ratatoskr's recovery code
matches on three substrings to be robust to wording changes:
`"410"`, `"GONE"`, `"syncToken"`. The mock's 410 envelope contains
"syncToken expired or not recognised", which hits the third
match.

Once recovery fires, ratatoskr clears the saved token and retries
without `syncToken`, expecting a full bootstrap response with a
fresh `nextSyncToken` on the final page. The mock honours this:

- Token unknown / not the seed and not the current `state` → 410.
- Token equals current `state` → empty connections list, same
  token echoed as `nextSyncToken`.
- No token → full list, paged via `nextPageToken`, terminating
  page carries `nextSyncToken`.

## Things that WILL break sync if wrong

- A pre-final page that emits `nextSyncToken`. ratatoskr would
  persist it before processing the next page.
- A 200 (instead of 410) on an unknown sync token. ratatoskr
  would try to apply the empty delta as the truth and lose
  contacts.
- Returning `{}` instead of `{"connections": [], ...}` on the
  delta path. ratatoskr's serde derives expect the field to be
  present (or `None`); a fully-empty body fails to parse.

## Mutations (write-back)

ratatoskr's contact editor pushes phone / company / notes back to
Google through the People API custom verbs. The mock implements
the request shape and bumps the change log; durable storage of
the patched fields isn't there because the fixture `Contact`
schema doesn't carry them.

- `PATCH /v1/people/{id}:updateContact?updatePersonFields=...`.
  Body is a partial `Person` (`{etag, phoneNumbers, organizations,
  biographies}`). Mock validates the contact exists, records a
  `contact_updated` transition, and echoes the (current) Person
  back. The next `connections` delta surfaces the contact id in
  the updated set.
- `DELETE /v1/people/{id}:deleteContact`. Mock validates the
  contact exists, removes it from `Fixture::contacts`, records a
  `contact_destroyed` transition (with the contact's
  `folder_id` as the destroyed parent), and returns `{}`. The
  next `connections` delta surfaces the contact id as a
  `metadata.deleted: true` tombstone.

The custom verbs arrive on `/v1/people/{id}:updateContact` /
`/v1/people/{id}:deleteContact` - the colon-prefixed verb is part
of the path segment, not a query string.

## What v0 doesn't surface

- `POST /v1/people:createContact`. ratatoskr doesn't yet create
  Google contacts (only existing-contact write-back).
- `otherContacts` actual data; the mock always returns an empty
  list because the fixture format has no `[other_contact]` table
  yet. Wire when a fixture needs it.
- Photo URLs. The mock omits the `photos[]` field entirely,
  which ratatoskr handles as "no avatar".
