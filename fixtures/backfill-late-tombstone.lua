-- Late tombstone: an id the backfill already delivered is retracted by
-- the live feed while the walk is still running.
--
-- READ THIS BEFORE USING IT AS A RACE GATE.
--
-- What this stages: ordering. The consumer is handed msg-3 by a
-- backfill page, and only afterwards told the server no longer has it.
-- Correct handling is last-writer-wins on arrival order.
--
-- What this does NOT stage: the resurrection race, where a backfill
-- emission for a DELETED object is suppressed because the live feed
-- got there first. That race needs the inventory to still YIELD the
-- id after the feed has announced its destruction - the case a
-- cold-start supersedes set exists for is literally "the inventory
-- snapshot was taken before the deletion". This mock's listing is
-- live, not a snapshot: once a message is destroyed it stops being
-- listed, so no later page ever offers it and the suppression path is
-- never entered. Announcing EARLIER would not fix that; it would only
-- make the id vanish from the walk sooner.
--
-- Reaching that race needs a mock affordance this repo does not have:
-- a stale inventory, where a destroyed object keeps appearing in
-- listings for a bounded number of pages after the change feed has
-- retracted it. That is a real server behaviour (eventually-consistent
-- listing versus an immediate change feed), so it is buildable - it is
-- simply not built, and this fixture should not be read as covering
-- it.
--
-- Ordering, maxResults=2, most-recent first:
--
--   page 1  offset 0  -> msg-5, msg-4
--   page 2  offset 2  -> msg-3, msg-2      <- msg-3 delivered here
--   [trigger: msg-3 destroyed + announced on the feed]
--   page 3  offset 4  -> msg-0
--
-- msg-1 is skipped by the walk. That is not a mock artifact to be
-- fixed - it is what offset pagination DOES when a row before the
-- cursor disappears mid-walk, and real offset-paged backfills have the
-- same hole. Pinned so a change to it is a decision rather than a
-- surprise.

fixture({ name = "backfill-late-tombstone", state = "blt-0" })
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
    id = "retract-3",
    email_destroy = { "msg-3" },
})

announce({
    step = "retract-3",
    before = "GET /gmail/v1/users/me/messages",
    nth = 3,
})
