#![allow(clippy::unwrap_used)]

//! End-to-end test that the Lua scenario loader produces a `Fixture`
//! byte-identical to the equivalent TOML fixture. With this guarantee,
//! any test that validates wire output against the TOML fixture also
//! validates the Lua loader for free.

use std::path::Path;

use saehrimnir::{fixture, lua};

#[test]
fn dispatcher_invokes_registered_callback() {
    // Smallest dispatcher smoke test: load a scenario that
    // registers `on("test", "ping", ...)`, then dispatch a "test"/
    // "ping" event and verify the callback fired.
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "cb" })
        account({ id = "a", name = "a@b" })
        on("test", "ping", function(req)
            return { status = "PONG", message = "hello" }
        end)
        "#,
        "@cb",
    )
    .unwrap();

    let result = dispatcher.dispatch("test", "ping", |_state| Ok(()));
    match result {
        lua::Override::Tagged { status, message } => {
            assert_eq!(status, "PONG");
            assert_eq!(message, "hello");
        }
        lua::Override::None => panic!("dispatch returned None - handler never fired"),
    }
}

#[test]
fn dispatcher_returns_none_when_no_handler_registered() {
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "no-cb" })
        account({ id = "a", name = "a@b" })
        "#,
        "@no-cb",
    )
    .unwrap();

    let result = dispatcher.dispatch("test", "ping", |_state| Ok(()));
    assert!(matches!(result, lua::Override::None));
}

#[test]
fn dispatcher_call_index_increments_per_protocol_command() {
    // Each dispatch increments req.call_index for that exact
    // (protocol, command) pair. The script branches on it to
    // return distinguishable status tokens.
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "ix" })
        account({ id = "a", name = "a@b" })
        on("test", "ping", function(req)
            if req.call_index == 1 then
                return { status = "FIRST", message = "x" }
            elseif req.call_index == 2 then
                return { status = "SECOND", message = "x" }
            else
                return { status = "LATER", message = "x" }
            end
        end)
        "#,
        "@ix",
    )
    .unwrap();

    let expected = ["FIRST", "SECOND", "LATER", "LATER"];
    for want in expected {
        let result = dispatcher.dispatch("test", "ping", |_state| Ok(()));
        match result {
            lua::Override::Tagged { status, .. } => assert_eq!(status, want),
            lua::Override::None => panic!("dispatch returned None"),
        }
    }
}

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
fn bulk_threads_default_three_messages_per_thread() {
    let fix = lua::load_source(
        r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({ count = 5, mailbox = "mb", seed = 9 })
        "#,
        "@thr",
    )
    .unwrap();
    assert_eq!(fix.emails.len(), 5 * 3);

    // Every thread shares one thread_id.
    let mut by_thread: std::collections::BTreeMap<String, Vec<&_>> = Default::default();
    for em in &fix.emails {
        by_thread.entry(em.thread_id.clone()).or_default().push(em);
    }
    assert_eq!(by_thread.len(), 5);
    for (_, msgs) in by_thread {
        assert_eq!(msgs.len(), 3);
    }
}

#[test]
fn bulk_threads_chains_in_reply_to_and_references() {
    let fix = lua::load_source(
        r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({
            count = 1,
            mailbox = "mb",
            messages_per_thread = 4,
            seed = 1,
        })
        "#,
        "@thr",
    )
    .unwrap();
    let msgs = &fix.emails;
    assert_eq!(msgs.len(), 4);
    let m0_id = &msgs[0].message_id[0];
    let m1_id = &msgs[1].message_id[0];
    let m2_id = &msgs[2].message_id[0];

    // First message: no in_reply_to / references.
    assert!(msgs[0].in_reply_to.is_empty());
    assert!(msgs[0].references.is_empty());
    // Second: replies to first; references = [m0].
    assert_eq!(msgs[1].in_reply_to, vec![m0_id.clone()]);
    assert_eq!(msgs[1].references, vec![m0_id.clone()]);
    // Third: replies to second; references = [m0, m1].
    assert_eq!(msgs[2].in_reply_to, vec![m1_id.clone()]);
    assert_eq!(msgs[2].references, vec![m0_id.clone(), m1_id.clone()]);
    // Fourth: replies to third; references = [m0, m1, m2].
    assert_eq!(msgs[3].in_reply_to, vec![m2_id.clone()]);
    assert_eq!(
        msgs[3].references,
        vec![m0_id.clone(), m1_id.clone(), m2_id.clone()]
    );
}

#[test]
fn bulk_threads_subjects_use_re_prefix_on_replies() {
    let fix = lua::load_source(
        r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({ count = 1, mailbox = "mb", messages_per_thread = 3 })
        "#,
        "@thr",
    )
    .unwrap();
    let s0 = fix.emails[0].subject.as_ref().unwrap();
    let s1 = fix.emails[1].subject.as_ref().unwrap();
    let s2 = fix.emails[2].subject.as_ref().unwrap();
    assert!(!s0.starts_with("Re: "));
    assert_eq!(s1, &format!("Re: {s0}"));
    assert_eq!(s2, &format!("Re: {s0}"));
}

#[test]
fn bulk_threads_seed_determinism() {
    let script = r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({ count = 10, mailbox = "mb", seed = 42 })
    "#;
    let a = lua::load_source(script, "@thr").unwrap();
    let b = lua::load_source(script, "@thr").unwrap();
    assert_eq!(a, b);
}

#[test]
fn bulk_threads_received_at_monotonic_within_and_across_threads() {
    let fix = lua::load_source(
        r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({
            count = 3,
            mailbox = "mb",
            messages_per_thread = 2,
            thread_interval_seconds = 7200,
            reply_interval_seconds = 600,
        })
        "#,
        "@thr",
    )
    .unwrap();
    // Within each thread, the second message is 600s after the first.
    // Each thread starts 7200s after the previous.
    let ts: Vec<_> = fix.emails.iter().map(|e| e.received_at).collect();
    assert_eq!((ts[1] - ts[0]).num_seconds(), 600);
    assert_eq!((ts[3] - ts[2]).num_seconds(), 600);
    assert_eq!((ts[2] - ts[0]).num_seconds(), 7200);
}

#[test]
fn bulk_threads_zero_messages_per_thread_errors() {
    let err = lua::load_source(
        r#"
        fixture({ name = "thr" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({ count = 1, mailbox = "mb", messages_per_thread = 0 })
        "#,
        "@thr",
    )
    .unwrap_err();
    assert!(err.contains("messages_per_thread"), "got: {err}");
}

#[test]
fn bulk_emails_count_above_max_errors() {
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_emails({ count = 9000000000, mailbox = "mb" })
        "#,
        "@bulk",
    )
    .unwrap_err();
    assert!(err.contains("MAX_BULK_COUNT"), "got: {err}");
}

#[test]
fn bulk_emails_negative_interval_overflow_caught_clean() {
    // Combining a large count with an obscene interval exceeds
    // chrono's range; should error cleanly, not panic.
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_emails({
            count = 1000000,
            mailbox = "mb",
            interval_seconds = 9223372036854775000,
        })
        "#,
        "@bulk",
    )
    .unwrap_err();
    assert!(err.contains("range") || err.contains("chrono"), "got: {err}");
}

#[test]
fn bulk_threads_count_above_max_errors() {
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        bulk_threads({ count = 9000000000, mailbox = "mb" })
        "#,
        "@thr",
    )
    .unwrap_err();
    assert!(err.contains("MAX_BULK_COUNT"), "got: {err}");
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
fn wait_helper_blocks_for_at_least_the_requested_duration() {
    let start = std::time::Instant::now();
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "w" })
        account({ id = "a", name = "a@b" })
        on("test", "ping", function(req)
            wait(50)
            return { status = "OK", message = "slept" }
        end)
        "#,
        "@wait",
    )
    .unwrap();
    let result = dispatcher.dispatch("test", "ping", |_state| Ok(()));
    let elapsed = start.elapsed();
    assert!(matches!(result, lua::Override::Tagged { .. }));
    assert!(elapsed >= std::time::Duration::from_millis(50), "got: {elapsed:?}");
}

#[test]
fn wait_helper_rejects_negative_argument() {
    let err = lua::load_source(
        r#"
        fixture({ name = "w" })
        account({ id = "a", name = "a@b" })
        wait(-1)
        "#,
        "@wait-neg",
    )
    .unwrap_err();
    assert!(err.contains("non-negative"), "got: {err}");
}

#[test]
fn mock_done_at_load_time_surfaces_via_dispatcher() {
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "d" })
        account({ id = "a", name = "a@b" })
        mock_done()
        "#,
        "@done",
    )
    .unwrap();
    assert_eq!(dispatcher.current_exit(), Some(lua::MockExit::Done));
}

#[test]
fn mock_fail_at_load_time_surfaces_with_reason() {
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "f" })
        account({ id = "a", name = "a@b" })
        mock_fail("missing fixture preset")
        "#,
        "@fail",
    )
    .unwrap();
    assert_eq!(
        dispatcher.current_exit(),
        Some(lua::MockExit::Fail("missing fixture preset".to_string()))
    );
}

#[test]
fn mock_done_inside_callback_is_observable_after_dispatch() {
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "cbd" })
        account({ id = "a", name = "a@b" })
        on("test", "ping", function(req)
            mock_done()
            return { status = "OK", message = "bye" }
        end)
        "#,
        "@cb-done",
    )
    .unwrap();
    assert_eq!(dispatcher.current_exit(), None);
    let _ = dispatcher.dispatch("test", "ping", |_state| Ok(()));
    assert_eq!(dispatcher.current_exit(), Some(lua::MockExit::Done));
}

#[test]
fn mock_done_then_mock_fail_first_call_wins() {
    let (_fixture, dispatcher) = lua::load_source_with_dispatcher(
        r#"
        fixture({ name = "fcw" })
        account({ id = "a", name = "a@b" })
        mock_done()
        mock_fail("too late")
        "#,
        "@fcw",
    )
    .unwrap();
    assert_eq!(dispatcher.current_exit(), Some(lua::MockExit::Done));
}

#[test]
fn mock_fail_requires_string_reason() {
    let err = lua::load_source(
        r#"
        fixture({ name = "x" })
        account({ id = "a", name = "a@b" })
        mock_fail(123)
        "#,
        "@bad-fail",
    )
    .unwrap_err();
    assert!(err.contains("reason"), "got: {err}");
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
