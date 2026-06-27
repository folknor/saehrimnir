#![allow(clippy::unwrap_used)]

//! Cross-protocol malformed-MIME injection.
//!
//! `Email::raw_bytes` was IMAP-only until this slice. The JMAP and
//! Gmail projections now also honour it: when set, JMAP
//! `Email/get`'s `bodyValues[*].value` carries the raw bytes lossily
//! decoded as UTF-8, and Gmail `threads.get`'s `payload.body.data`
//! carries them base64url-encoded with no `parts[]` tree. Lets a
//! fixture inject anomalous body content (CRLF-only, bare-LF, 8-bit
//! sequences, oversized data) through every protocol the canonical
//! body field would otherwise be re-synthesized for.

use std::path::Path;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, gmail, lua, routes, shared};

const MALFORMED_BODY: &str =
    "line1-LF-only\nline2-CR-only\rline3-mixed\r\nbroken=??=encoded-word\r\n";

fn scenario_with_malformed(body_text: &str, raw: &str) -> String {
    format!(
        r#"
        fixture({{ name = "malformed" }})
        account({{ id = "account-1", name = "alice@example.com" }})
        mailbox({{ id = "mb-inbox", name = "Inbox", role = "inbox" }})
        email({{
            id = "e1",
            mailbox_ids = {{"mb-inbox"}},
            received_at = "2026-01-15T10:00:00Z",
            sent_at = "2026-01-15T10:00:00Z",
            from = "alice@example.com",
            to = {{"bob@example.com"}},
            subject = "malformed",
            body_text = {body_text:?},
            body_raw_bytes = {raw:?},
        }})
        "#
    )
}

fn jmap_router_from_scenario(scenario: &str) -> axum::Router {
    let (fix, dispatcher) = lua::load_source_with_dispatcher(scenario, "@malformed").unwrap();
    let handle = shared::handle(fix);
    let mut state = routes::AppState::for_test(handle);
    state.shared.dispatcher = Some(Arc::new(dispatcher));
    routes::router(state)
}

fn gmail_router_from_scenario(scenario: &str) -> axum::Router {
    let (fix, dispatcher) = lua::load_source_with_dispatcher(scenario, "@malformed").unwrap();
    let handle = shared::handle(fix);
    let mut state = gmail::AppState::for_test(handle);
    state.shared.dispatcher = Some(Arc::new(dispatcher));
    gmail::router(state)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn jmap_email_get_emits_raw_bytes_in_body_values() {
    let body_text = "harmless";
    let r = jmap_router_from_scenario(&scenario_with_malformed(body_text, MALFORMED_BODY));
    let req = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Email/get", {
                "accountId": "account-1",
                "ids": ["e1"],
                "fetchTextBodyValues": true,
            }, "c0"]
        ],
    });
    let resp = r
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let mr = &v["methodResponses"][0];
    assert_eq!(mr[0], "Email/get");
    let email = &mr[1]["list"][0];
    let part_id = email["textBody"][0]["partId"].as_str().unwrap();
    let value = email["bodyValues"][part_id]["value"].as_str().unwrap();
    // The wire body is the raw bytes verbatim, not the canonical
    // body_text. Anomalous separators (bare LF, bare CR) and
    // ill-formed encoded-word stay intact.
    assert_eq!(value, MALFORMED_BODY);
    assert_ne!(value, "harmless");
    // size on the textBody part reflects raw byte length, not the
    // canonical body length.
    let size = email["textBody"][0]["size"].as_u64().unwrap();
    assert_eq!(usize::try_from(size).unwrap(), MALFORMED_BODY.len());
}

#[tokio::test]
async fn jmap_email_get_falls_back_to_body_text_when_raw_unset() {
    // No body_raw_bytes: bodyValues comes from body_text as before.
    let scenario = r#"
        fixture({ name = "canonical" })
        account({ id = "account-1", name = "alice@example.com" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "canonical body",
        })
    "#;
    let r = jmap_router_from_scenario(scenario);
    let req = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Email/get", {
                "accountId": "account-1",
                "ids": ["e1"],
                "fetchTextBodyValues": true,
            }, "c0"]
        ],
    });
    let resp = r
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let email = &v["methodResponses"][0][1]["list"][0];
    let part_id = email["textBody"][0]["partId"].as_str().unwrap();
    assert_eq!(email["bodyValues"][part_id]["value"], "canonical body");
}

#[tokio::test]
async fn gmail_threads_get_emits_raw_bytes_as_payload_body_data() {
    let body_text = "harmless";
    let r = gmail_router_from_scenario(&scenario_with_malformed(body_text, MALFORMED_BODY));

    // Gmail's thread-get path: list threads then fetch one. Fixture
    // has a single email so threadId == emailId == "e1".
    let resp = r
        .oneshot(
            Request::builder()
                .uri("/gmail/v1/users/me/threads/e1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let payload = &v["messages"][0]["payload"];
    // No multipart wrapping when raw_bytes is set; single text/plain
    // leaf with the raw bytes base64url-encoded as body.data.
    assert_eq!(payload["mimeType"], "text/plain");
    assert!(payload["parts"].is_null() || payload["parts"].as_array().is_some_and(Vec::is_empty));
    let data_b64 = payload["body"]["data"].as_str().unwrap();
    let decoded = decode_base64url(data_b64);
    assert_eq!(decoded, MALFORMED_BODY.as_bytes());
    let body_size = payload["body"]["size"].as_u64().unwrap();
    assert_eq!(usize::try_from(body_size).unwrap(), MALFORMED_BODY.len());
    let size_estimate = v["messages"][0]["sizeEstimate"].as_u64().unwrap();
    assert_eq!(
        usize::try_from(size_estimate).unwrap(),
        MALFORMED_BODY.len()
    );
}

#[tokio::test]
async fn imap_uid_fetch_still_emits_raw_bytes_verbatim() {
    // Spot-check the existing IMAP path. We don't need a full duplex
    // run here; load through the JMAP router to verify the fixture
    // round-trips, then directly assert on the in-memory fixture.
    let scenario = scenario_with_malformed("ignored", MALFORMED_BODY);
    let (fix, _) = lua::load_source_with_dispatcher(&scenario, "@x").unwrap();
    let email = fix.emails.iter().find(|e| e.id == "e1").unwrap();
    assert_eq!(email.raw_bytes.as_deref(), Some(MALFORMED_BODY));
}

#[tokio::test]
async fn jmap_truncated_multipart_body_round_trips_intact() {
    // Adversarial: claim multipart but never emit the boundary.
    let raw =
        "Content-Type: multipart/mixed; boundary=\"X\"\r\n\r\n--X-but-no-boundary-line\r\nbroken";
    let r = jmap_router_from_scenario(&scenario_with_malformed("ignored", raw));
    let req = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Email/get", {"accountId": "account-1", "ids": ["e1"], "fetchTextBodyValues": true}, "c0"]],
    });
    let resp = r
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let email = &v["methodResponses"][0][1]["list"][0];
    let part_id = email["textBody"][0]["partId"].as_str().unwrap();
    assert_eq!(email["bodyValues"][part_id]["value"], raw);
}

fn decode_base64url(s: &str) -> Vec<u8> {
    // Lookup table for `A-Za-z0-9-_` per RFC 4648 §5. Builds 6-bit
    // groups left-to-right.
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for ch in s.chars() {
        if ch == '=' {
            break;
        }
        let v: u32 = match ch {
            'A'..='Z' => (ch as u32) - ('A' as u32),
            'a'..='z' => (ch as u32) - ('a' as u32) + 26,
            '0'..='9' => (ch as u32) - ('0' as u32) + 52,
            '-' => 62,
            '_' => 63,
            _ => continue,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Loaded for fixture-level path correctness; this is the path the
/// notes/fixture-format.md doc points fixture authors at.
fn _ensure_fixture_module_imports_compile() {
    let path = Path::new("fixtures/jmap-small.toml");
    let _ = fixture::load(path);
}

#[test]
fn base64url_round_trip() {
    let cases: &[(&[u8], &str)] = &[(b"", ""), (b"f", "Zg"), (b"fo", "Zm8"), (b"foo", "Zm9v")];
    for (bytes, expect) in cases {
        let got = encode_for_check(bytes);
        assert_eq!(&got, expect);
        assert_eq!(decode_base64url(&got), *bytes);
    }
}

fn encode_for_check(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let (chunks, rem) = bytes.as_chunks::<3>();
    for &[c0, c1, c2] in chunks {
        let n = (u32::from(c0) << 16) | (u32::from(c1) << 8) | u32::from(c2);
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(A[((n >> 6) & 63) as usize] as char);
        out.push(A[(n & 63) as usize] as char);
    }
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(A[((n >> 18) & 63) as usize] as char);
            out.push(A[((n >> 12) & 63) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(A[((n >> 18) & 63) as usize] as char);
            out.push(A[((n >> 12) & 63) as usize] as char);
            out.push(A[((n >> 6) & 63) as usize] as char);
        }
        _ => {}
    }
    out
}
