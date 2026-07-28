-- Cold-start backfill race, CREATED arm.
--
-- A consumer opening an account walks a paginated inventory while a
-- live change feed runs alongside it. The shape staged here is the
-- double-emit: an object the feed announces WHILE the walk is in
-- flight, which the walk then also serves, must be ingested exactly
-- once.
--
-- `POST /test/fixture/step` cannot stage this. It only interleaves
-- between whole request/response pairs, and the consumer decides when
-- it asks, so a harness can put a change before or after a backfill
-- but never DURING one. The `announce({...})` trigger applies the step
-- (and pushes it on the change feed) inside the listener, before the
-- handler for the second page request runs.
--
-- Ordering, with maxResults=2 over the six baseline messages (Gmail
-- lists most-recent first, so msg-5 down to msg-0):
--
--   page 1  offset 0  -> msg-5, msg-4
--   [trigger: msg-new created + announced on the feed]
--   page 2  offset 2  -> msg-3, msg-2
--   page 3  offset 4  -> msg-1, msg-0
--   page 4  offset 6  -> msg-new
--
-- msg-new is dated OLDER than every baseline message on purpose. It
-- sorts to the tail, so the insert APPENDS rather than shifting the
-- offsets of pages the walk has not reached - the arm stays about the
-- double-emit and does not accidentally also stage the offset-shift
-- hazard (see the DESTROYED arm, which cannot avoid it). The consumer
-- ends up handed msg-new from both sides: the feed at page 2, the
-- backfill at page 4.

fixture({ name = "backfill-race-created", state = "brc-0" })
account({ id = "account-1", name = "test@example.com" })

mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })

bulk_emails({
    count = 6,
    mailbox = "mb-inbox",
    seed = 7,
    start_at = "2026-04-01T10:00:00Z",
    interval_seconds = 3600,
    id_prefix = "msg",
})

change({
    id = "arrive-new",
    email_create = {
        {
            id = "msg-new",
            thread_id = "thread-new",
            mailbox_ids = { "mb-inbox" },
            received_at = "2026-04-01T09:00:00Z",
            from = "dave@example.com",
            to = { "test@example.com" },
            subject = "Arrived mid-backfill",
            body_text = "Landed while the walk was in flight.",
            message_id = { "<new@example.com>" },
        },
    },
})

-- `before` matches a prefix of the request LINE ("<METHOD> <path>",
-- query excluded), so it also matches
-- `GET /gmail/v1/users/me/messages/{id}` hydration reads. A gate that
-- hydrates between pages has to count those when picking `nth`.
announce({
    step = "arrive-new",
    before = "GET /gmail/v1/users/me/messages",
    nth = 2,
})
