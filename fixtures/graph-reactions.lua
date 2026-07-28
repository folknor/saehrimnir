-- Exchange message-reaction scenario. Three baseline messages cover
-- the three authorable reaction states (owner + count, count only,
-- neither), and a three-step change script drives one message through
-- add -> change -> remove so a consumer's reaction reconciliation is
-- observable across delta cycles.
--
-- Reactions surface on the Graph `singleValueExtendedProperties`
-- read, selected by a `$filter` on the GUID-qualified property ids.
-- Asserted byte-equivalent to `graph-reactions.toml` in
-- `tests/lua_fixture.rs`.

fixture({ name = "graph-reactions", state = "rx-0" })
account({ id = "account-1", name = "test@example.com" })

mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })

-- Owner reacted AND others did: both extended properties present.
email({
    id = "email-001",
    mailbox_ids = { "mb-inbox" },
    keywords = { "$seen" },
    received_at = "2026-01-15T10:00:00Z",
    from = { name = "Alice", email = "alice@example.com" },
    to = { "test@example.com" },
    subject = "Welcome",
    body_text = "Hello there.",
    message_id = { "<001@example.com>" },
    reaction_type = "like",
    reaction_count = 3,
})

-- Others reacted, the owner did not: count only, no owner type. The
-- two properties are independent slots, not a pair.
email({
    id = "email-002",
    mailbox_ids = { "mb-inbox" },
    received_at = "2026-01-15T11:00:00Z",
    from = "bob@example.com",
    to = { "test@example.com" },
    subject = "Status update",
    body_text = "Things are progressing.",
    message_id = { "<002@example.com>" },
    reaction_count = 2,
})

-- Nobody reacted: neither property exists on the item.
email({
    id = "email-003",
    mailbox_ids = { "mb-inbox" },
    received_at = "2026-01-15T12:00:00Z",
    from = "carol@example.com",
    to = { "test@example.com" },
    subject = "Lunch?",
    body_text = "Free at 12:30?",
    message_id = { "<003@example.com>" },
})

-- Step 1: the owner reacts to a message that had no reaction.
change({
    id = "react",
    email_reaction = {
        { id = "email-003", reaction_type = "heart", reaction_count = 1 },
    },
})

-- Step 2: the owner swaps their reaction and a second person joins.
change({
    id = "rereact",
    email_reaction = {
        { id = "email-003", reaction_type = "laugh", reaction_count = 2 },
    },
})

-- Step 3: the reaction is removed entirely. Both properties go back to
-- absent - not present-with-empty-value, which is a different state to
-- a consumer reconciling reaction rows.
change({
    id = "unreact",
    email_reaction = {
        { id = "email-003", clear = true },
    },
})
