-- Gmail bulk-mutation scenario.
--
-- Stages the shape a source-carrying bulk move drives: ONE
-- `messages.batchModify` that carries both the destination it is
-- adding and the container it is leaving, instead of a move followed
-- by a per-message detach. The mock has to honour add-and-remove in a
-- single request, and has to get the mutually exclusive system
-- containers right - a move into the inbox is an un-spam / un-trash,
-- so SPAM and TRASH come off in the same patch that puts INBOX on.
--
-- Every container the bulk paths can name is declared, precisely
-- because the mock now REFUSES an `addLabelIds` naming a container the
-- fixture has no mailbox for. A fixture missing `mb-spam` would turn a
-- consumer's un-spam into a silent partial patch, which is the
-- wrong-shape trap this file exists to keep out of the gate.
--
--   mb-inbox    role inbox    -> Gmail INBOX
--   mb-spam     role junk     -> Gmail SPAM
--   mb-trash    role trash    -> Gmail TRASH
--   mb-archive  role archive  -> NO Gmail label (All Mail). This is
--                               where an archived message lands: real
--                               Gmail leaves an archived message with
--                               no container label at all, and this
--                               mailbox is how the fixture format
--                               spells that state.
--   mb-work     roleless      -> Gmail user label Label_mb-work

fixture({ name = "gmail-bulk-move", state = "bm-0" })
account({ id = "account-1", name = "test@example.com" })

mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })
mailbox({ id = "mb-spam", name = "Spam", role = "junk", sort_order = 1 })
mailbox({ id = "mb-trash", name = "Trash", role = "trash", sort_order = 2 })
mailbox({ id = "mb-archive", name = "Archive", role = "archive", sort_order = 3 })
mailbox({ id = "mb-work", name = "Work", sort_order = 4 })

-- Two plain inbox messages: the bulk-move targets. Distinct threads so
-- a message-scoped patch cannot be confused with a thread-wide one.
email({
    id = "msg-1",
    thread_id = "thread-1",
    mailbox_ids = { "mb-inbox" },
    keywords = { "$seen" },
    received_at = "2026-04-01T09:00:00Z",
    from = { name = "Alice", email = "alice@example.com" },
    to = { "test@example.com" },
    subject = "Invoice",
    body_text = "Attached in spirit.",
    message_id = { "<1@example.com>" },
})

email({
    id = "msg-2",
    thread_id = "thread-2",
    mailbox_ids = { "mb-inbox" },
    keywords = { "$seen" },
    received_at = "2026-04-01T10:00:00Z",
    from = "bob@example.com",
    to = { "test@example.com" },
    subject = "Contract",
    body_text = "Signed.",
    message_id = { "<2@example.com>" },
})

-- Already carries the destination label AND sits in the inbox, so a
-- move to Label_mb-work must be a pure detach for this one: the add is
-- a no-op and only the INBOX removal is observable.
email({
    id = "msg-3",
    thread_id = "thread-3",
    mailbox_ids = { "mb-inbox", "mb-work" },
    keywords = { "$seen" },
    received_at = "2026-04-01T11:00:00Z",
    from = "carol@example.com",
    to = { "test@example.com" },
    subject = "Standup notes",
    body_text = "Nothing blocking.",
    message_id = { "<3@example.com>" },
})

-- In SPAM. A move to INBOX has to strip SPAM in the same request:
-- merely adding INBOX leaves the message displayed as spam.
email({
    id = "msg-spam",
    thread_id = "thread-spam",
    mailbox_ids = { "mb-spam" },
    keywords = {},
    received_at = "2026-04-01T12:00:00Z",
    from = "spammer@example.com",
    to = { "test@example.com" },
    subject = "You have won",
    body_text = "Click here.",
    message_id = { "<spam@example.com>" },
})

-- In TRASH. Same shape as msg-spam for the other exclusive container.
email({
    id = "msg-trash",
    thread_id = "thread-trash",
    mailbox_ids = { "mb-trash" },
    keywords = { "$seen" },
    received_at = "2026-04-01T13:00:00Z",
    from = "dave@example.com",
    to = { "test@example.com" },
    subject = "Old thread",
    body_text = "Discarded.",
    message_id = { "<trash@example.com>" },
})
