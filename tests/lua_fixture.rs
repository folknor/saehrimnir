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
