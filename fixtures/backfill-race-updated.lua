-- Cold-start backfill race, UPDATED arm. The negative control.
--
-- An `Updated` on the live feed must NOT suppress the backfill's own
-- emission of that object. A de-dup set that swallows anything the
-- feed mentioned is wrong here: an update announces a CHANGE to an
-- object, and during a cold start the consumer may hold no row for it
-- yet, so dropping the backfill emission loses the object entirely.
-- Only `Created` (the object is fully described by the feed) and
-- `Destroyed` (the object should not exist) may suppress it.
--
-- Ordering, maxResults=2, most-recent first:
--
--   page 1  offset 0  -> msg-5, msg-4
--   [trigger: msg-1 starred + announced on the feed]
--   page 2  offset 2  -> msg-3, msg-2
--   page 3  offset 4  -> msg-1, msg-0      <- msg-1 emitted here
--
-- The trigger fires before page 2, so the feed announces msg-1 while
-- the walk is still two pages away from it. An in-place update
-- neither adds nor removes a row and leaves `received_at` alone, so
-- unlike the DESTROYED arm no offset shifts and the walk serves all
-- six messages.

fixture({ name = "backfill-race-updated", state = "bru-0" })
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

-- Starring one message is a label-set delta, so `history.list`
-- projects a precise `labelsAdded` naming msg-1 alone.
change({
    id = "touch-1",
    email_update = {
        { id = "msg-1", keywords = { "$seen", "$flagged" } },
    },
})

announce({
    step = "touch-1",
    before = "GET /gmail/v1/users/me/messages",
    nth = 2,
})
