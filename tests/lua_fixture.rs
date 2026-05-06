//! End-to-end test that the Lua scenario loader produces a `Fixture`
//! byte-identical to the equivalent TOML fixture. With this guarantee,
//! any test that validates wire output against the TOML fixture also
//! validates the Lua loader for free.

use std::path::Path;

use saehrimnir::{fixture, lua};

#[test]
fn lua_fixture_matches_equivalent_toml() {
    let from_toml = fixture::load(Path::new("fixtures/jmap-small.toml")).unwrap();
    let from_lua = fixture::load(Path::new("fixtures/jmap-small.lua")).unwrap();
    assert_eq!(from_toml, from_lua);
}

#[test]
fn lua_loader_rejects_missing_fixture_call() {
    let err = lua::load_source(
        r#"account({ id = "a", name = "a@b" })"#,
        "@test",
    )
    .unwrap_err();
    assert!(err.contains("fixture"), "unexpected error: {err}");
}

#[test]
fn lua_loader_rejects_missing_account_call() {
    let err = lua::load_source(
        r#"fixture({ name = "x" })"#,
        "@test",
    )
    .unwrap_err();
    assert!(err.contains("account"), "unexpected error: {err}");
}

#[test]
fn lua_loader_propagates_normalize_errors() {
    // mailbox_ids references a mailbox that doesn't exist - normalize
    // should reject.
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        email({
            id = "e1",
            mailbox_ids = {"ghost"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "x",
        })
        "#,
        "@test",
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "unexpected error: {err}");
}

#[test]
fn lua_loader_supports_table_address_form() {
    let fix = lua::load_source(
        r#"
        fixture({ name = "addr" })
        account({ id = "a", name = "alice@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        email({
            id = "e",
            mailbox_ids = {"mb"},
            from = { name = "Alice", email = "alice@example.com" },
            to = {{ name = "Bob", email = "bob@example.com" }},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
        "#,
        "@test",
    )
    .unwrap();
    let from = fix.emails[0].from.as_ref().unwrap();
    assert_eq!(from.name.as_deref(), Some("Alice"));
    assert_eq!(from.email, "alice@example.com");
    let to = &fix.emails[0].to[0];
    assert_eq!(to.name.as_deref(), Some("Bob"));
    assert_eq!(to.email, "bob@example.com");
}

#[test]
fn bulk_emails_generates_count_and_is_seed_deterministic() {
    let script = r#"
        fixture({ name = "bulk" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_emails({ count = 100, mailbox = "mb", seed = 7 })
    "#;
    let a = lua::load_source(script, "@bulk").unwrap();
    let b = lua::load_source(script, "@bulk").unwrap();
    assert_eq!(a.emails.len(), 100);
    // Same seed -> byte-identical output across runs.
    assert_eq!(a, b);
    // First/last id zero-padded so lex order matches numeric order.
    assert_eq!(a.emails[0].id, "bulk-000");
    assert_eq!(a.emails[99].id, "bulk-099");
    // Each email belongs to the named mailbox and gets its own thread.
    for (i, em) in a.emails.iter().enumerate() {
        assert_eq!(em.mailbox_ids, vec!["mb".to_string()]);
        assert_eq!(em.thread_id, em.id);
        assert!(em.subject.is_some());
        assert!(em.from.is_some());
        // received_at increases monotonically.
        if i > 0 {
            assert!(em.received_at > a.emails[i - 1].received_at);
        }
    }
}

#[test]
fn bulk_emails_different_seeds_produce_different_subjects() {
    let mk = |seed: u64| {
        let src = format!(
            r#"
            fixture({{ name = "bulk" }})
            account({{ id = "a", name = "a@b" }})
            mailbox({{ id = "mb", name = "Inbox", role = "inbox" }})
            bulk_emails({{ count = 5, mailbox = "mb", seed = {seed} }})
            "#
        );
        lua::load_source(&src, "@bulk").unwrap()
    };
    let a = mk(1);
    let b = mk(2);
    let subj_a: Vec<_> = a.emails.iter().map(|e| e.subject.clone()).collect();
    let subj_b: Vec<_> = b.emails.iter().map(|e| e.subject.clone()).collect();
    assert_ne!(subj_a, subj_b);
}

#[test]
fn bulk_emails_pure_lua_loop_is_equivalent_for_explicit_emails() {
    // Pure-Lua-loop variant - not using bulk_emails. Validates the
    // existing email() builder scales fine for the small-N case.
    let src = r#"
        fixture({ name = "loop" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        for i = 1, 10 do
            email({
                id = "e" .. i,
                mailbox_ids = {"mb"},
                received_at = "2026-01-15T10:00:00Z",
                body_text = "body " .. i,
            })
        end
    "#;
    let fix = lua::load_source(src, "@loop").unwrap();
    assert_eq!(fix.emails.len(), 10);
    assert_eq!(fix.emails[0].id, "e1");
    assert_eq!(fix.emails[9].id, "e10");
}

#[test]
fn bulk_emails_validates_mailbox_at_normalize_time() {
    // bulk_emails accepts the mailbox id without checking it exists -
    // but normalize() catches the bad reference at the end.
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        bulk_emails({ count = 3, mailbox = "ghost" })
        "#,
        "@bulk",
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "unexpected error: {err}");
}

#[test]
fn lua_loader_double_account_call_errors() {
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        account({ id = "a2", name = "b@c" })
        "#,
        "@test",
    )
    .unwrap_err();
    assert!(err.contains("account"), "unexpected error: {err}");
}
