-- Gmail multi-message-thread scenario.
--
-- Stages the shape a consumer needs to gate the difference between a
-- MESSAGE-target and a THREAD-target label change:
--
--   thread-multi   three messages, one Gmail thread
--   thread-solo    one message, control (a thread-wide change here is
--                  indistinguishable from a message change, which is
--                  exactly why the multi-message thread is needed)
--   thread-trashed two messages already in Trash, so the permanent
--                  `threads.delete` path has something to destroy
--                  (bifrost only issues it for an already-trashed
--                  thread; anything else is a move-to-trash)
--
-- bifrost routes `MutationTarget::Message` to
-- `POST /messages/{id}/modify` and `MutationTarget::Thread` to
-- `POST /threads/{id}/modify`. Both land here, and `history.list`
-- projects one record per MESSAGE either way - so "I labelled one
-- message" and "I labelled the whole thread" are distinguishable in
-- the delta, and a gate can assert the siblings' membership survived
-- the first and did not survive the second.

fixture({ name = "gmail-threads", state = "gt-0" })
account({ id = "account-1", name = "test@example.com" })

mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })
mailbox({ id = "mb-trash", name = "Trash", role = "trash", sort_order = 1 })
mailbox({ id = "mb-archive", name = "Archive", role = "archive", sort_order = 2 })
-- A roleless mailbox projects as the Gmail user label `Label_mb-work`.
mailbox({ id = "mb-work", name = "Work", sort_order = 3 })

-- thread-multi: three messages, distinct label sets on purpose. The
-- siblings must be observably unequal to the mutated message, so a
-- "membership survived" assertion cannot pass by accident on a
-- uniform thread.
email({
    id = "msg-a",
    thread_id = "thread-multi",
    mailbox_ids = { "mb-inbox" },
    keywords = { "$seen" },
    received_at = "2026-03-02T09:00:00Z",
    from = { name = "Alice", email = "alice@example.com" },
    to = { "test@example.com" },
    subject = "Quarterly plan",
    body_text = "First draft attached in spirit.",
    message_id = { "<a@example.com>" },
})

email({
    id = "msg-b",
    thread_id = "thread-multi",
    mailbox_ids = { "mb-inbox", "mb-work" },
    keywords = { "$seen" },
    received_at = "2026-03-02T10:00:00Z",
    from = "test@example.com",
    to = { "alice@example.com" },
    subject = "Re: Quarterly plan",
    body_text = "Comments inline.",
    message_id = { "<b@example.com>" },
    in_reply_to = { "<a@example.com>" },
    references = { "<a@example.com>" },
})

-- Unread: its UNREAD label must survive a sibling's change untouched.
email({
    id = "msg-c",
    thread_id = "thread-multi",
    mailbox_ids = { "mb-inbox" },
    received_at = "2026-03-02T11:00:00Z",
    from = "bob@example.com",
    to = { "test@example.com" },
    subject = "Re: Quarterly plan",
    body_text = "Adding Bob.",
    message_id = { "<c@example.com>" },
    in_reply_to = { "<b@example.com>" },
    references = { "<a@example.com>", "<b@example.com>" },
})

-- thread-solo: single-message control thread.
email({
    id = "msg-solo",
    thread_id = "thread-solo",
    mailbox_ids = { "mb-inbox" },
    keywords = { "$seen" },
    received_at = "2026-03-01T08:00:00Z",
    from = "carol@example.com",
    to = { "test@example.com" },
    subject = "Standalone",
    body_text = "No replies.",
    message_id = { "<solo@example.com>" },
})

-- thread-trashed: already in Trash, so `DELETE /threads/{id}` is the
-- permanent destroy rather than a move.
email({
    id = "msg-t1",
    thread_id = "thread-trashed",
    mailbox_ids = { "mb-trash" },
    keywords = { "$seen" },
    received_at = "2026-02-20T07:00:00Z",
    from = "spam@example.com",
    to = { "test@example.com" },
    subject = "Old news",
    body_text = "Discarded.",
    message_id = { "<t1@example.com>" },
})

email({
    id = "msg-t2",
    thread_id = "thread-trashed",
    mailbox_ids = { "mb-trash" },
    keywords = { "$seen" },
    received_at = "2026-02-20T07:30:00Z",
    from = "spam@example.com",
    to = { "test@example.com" },
    subject = "Re: Old news",
    body_text = "Still discarded.",
    message_id = { "<t2@example.com>" },
})

-- Server-side variant of the same gate: the change arrives from the
-- server (another client starred one message) rather than from the
-- consumer's own mutation. `history.list` must name msg-b alone.
change({
    id = "star-one",
    email_update = {
        { id = "msg-b", keywords = { "$seen", "$flagged" } },
    },
})
