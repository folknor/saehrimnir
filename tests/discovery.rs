#![allow(clippy::unwrap_used)]

//! Tests for the account-discovery routes mounted on the JMAP
//! HTTP listener: WebFinger (`/.well-known/webfinger`), OIDC
//! discovery (`/.well-known/openid-configuration`), and Mozilla
//! autoconfig (`/mail/config-v1.1.xml`). See
//! `reference/ratatoskr-discovery-surface.md` for the wire contract.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use saehrimnir::{fixture, routes};

const BASE: &str = "http://localhost";

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/discovery-small.toml")).unwrap();
    routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn get(path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn get_json(path: &str) -> Value {
    let (status, _, bytes) = get(path).await;
    assert_eq!(status, StatusCode::OK, "GET {path}: status {status}");
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "GET {path}: body is not JSON: {e}; raw={:?}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

#[tokio::test]
async fn webfinger_serves_jrd_with_prefixed_href() {
    let v = get_json(
        "/corp.test/.well-known/webfinger\
         ?resource=acct:user@corp.test&rel=http://openid.net/specs/connect/1.0/issuer",
    )
    .await;
    assert_eq!(v["subject"], "acct:user@corp.test");
    let links = v["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0]["rel"],
        "http://openid.net/specs/connect/1.0/issuer"
    );
    assert_eq!(
        links[0]["href"],
        format!("{BASE}/idp/realms/corp"),
        "path-relative href must get the listener base URL prefixed at emit time",
    );
}

#[tokio::test]
async fn webfinger_content_type_is_jrd() {
    let (_, headers, _) =
        get("/corp.test/.well-known/webfinger?resource=acct:user@corp.test").await;
    let ct = headers.get(axum::http::header::CONTENT_TYPE).unwrap();
    assert_eq!(ct.to_str().unwrap(), "application/jrd+json");
}

#[tokio::test]
async fn webfinger_filters_by_rel_when_supplied() {
    let v = get_json("/corp.test/.well-known/webfinger?resource=acct:x&rel=does-not-match").await;
    assert!(v["links"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn webfinger_unknown_prefix_404s() {
    let (status, _, _) = get("/nope.test/.well-known/webfinger?resource=acct:x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webfinger_raw_body_passes_through_verbatim() {
    let (status, _, bytes) = get("/malformed-jrd.test/.well-known/webfinger?resource=acct:x").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(std::str::from_utf8(&bytes).unwrap(), "this is not json {{[");
}

#[tokio::test]
async fn webfinger_emits_absolute_http_href_verbatim() {
    // Negative-test contract: when the fixture stages an absolute
    // `http://` href, the handler must NOT rewrite it - ratatoskr's
    // scheme check needs to see the raw http:// in the response.
    let v = get_json("/insecure-href.test/.well-known/webfinger?resource=acct:x").await;
    assert_eq!(v["links"][0]["href"], "http://insecure.example/issuer",);
}

#[tokio::test]
async fn oidc_discovery_serves_full_document() {
    let v = get_json("/idp/realms/corp/.well-known/openid-configuration").await;
    assert_eq!(v["issuer"], format!("{BASE}/idp/realms/corp"));
    assert_eq!(
        v["authorization_endpoint"],
        format!("{BASE}/oauth/authorize")
    );
    assert_eq!(v["token_endpoint"], format!("{BASE}/oauth/token"));
    assert_eq!(v["userinfo_endpoint"], format!("{BASE}/oauth/userinfo"));
    let scopes = v["scopes_supported"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "openid"));
    assert!(scopes.iter().any(|s| s == "offline_access"));
}

#[tokio::test]
async fn oidc_discovery_with_mismatched_issuer_still_serves() {
    // The mock advertises whatever the fixture says, including a
    // deliberately-wrong issuer. The client (ratatoskr) is the one
    // that rejects the mismatch; sæhrimnir's job is to give the
    // client the staged document.
    let v = get_json("/wrong-issuer.test/.well-known/openid-configuration").await;
    assert_eq!(
        v["issuer"],
        format!("{BASE}/something-else"),
        "fixture-staged issuer must pass through unchanged so the \
         issuer-self-claim mismatch surfaces at the client",
    );
}

#[tokio::test]
async fn oidc_discovery_unknown_prefix_404s() {
    let (status, _, _) = get("/nope.test/.well-known/openid-configuration").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oidc_discovery_direct_at_domain_prefix() {
    // Stage 6's parallel direct probe: no WebFinger chain, just
    // hit `${BASE}/{domain}/.well-known/openid-configuration`.
    let v = get_json("/corp.test/.well-known/openid-configuration").await;
    assert_eq!(v["issuer"], format!("{BASE}/corp.test"));
}

#[tokio::test]
async fn autoconfig_serves_xml_with_base_substituted() {
    let (status, headers, bytes) = get("/corp.test/mail/config-v1.1.xml").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/xml"
    );
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(body.contains("<clientConfig version=\"1.1\">"));
    assert!(
        body.contains(&format!("<issuer>{BASE}/idp/realms/corp</issuer>")),
        "${{BASE}} must be replaced with the listener base URL"
    );
    assert!(!body.contains("${BASE}"), "no unsubstituted tokens");
}

#[tokio::test]
async fn autoconfig_unknown_prefix_404s() {
    let (status, _, _) = get("/nope.test/mail/config-v1.1.xml").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_path_with_no_known_suffix_404s() {
    // The `/{*discovery_path}` catch-all must not intercept paths
    // that don't end with one of the three discovery suffixes.
    let (status, _, _) = get("/something/random").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn known_jmap_routes_are_not_shadowed_by_catchall() {
    // The wildcard route must not intercept the well-known JMAP
    // endpoints. Specific literal segments win in axum's matcher.
    let (status, _, _) = get("/.well-known/jmap").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = get("/jmap/session").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn discovery_requests_land_in_the_request_log() {
    // Wire up an explicit request log so we can read it back.
    let fix = fixture::load(std::path::Path::new("fixtures/discovery-small.toml")).unwrap();
    let log = saehrimnir::request_log::RequestLog::default();
    let state =
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone());
    let r = routes::router(state);

    let _ = r
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/corp.test/.well-known/webfinger?resource=acct:x&rel=foo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = r
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/idp/realms/corp/.well-known/openid-configuration")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let entries = log.snapshot();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].protocol, "discovery");
    assert_eq!(entries[0].command, "webfinger");
    assert_eq!(entries[0].detail["prefix"], "corp.test");
    assert_eq!(entries[0].detail["resource"], "acct:x");
    assert_eq!(entries[0].detail["rel"], "foo");
    assert_eq!(entries[1].command, "openid-configuration");
    assert_eq!(entries[1].detail["prefix"], "idp/realms/corp");
}
