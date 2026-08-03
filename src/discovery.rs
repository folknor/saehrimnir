//! Account-discovery routes mounted on the JMAP HTTP listener.
//!
//! Three shapes, all unauthenticated (real WebFinger / OIDC
//! discovery / Mozilla autoconfig endpoints are public; bearer
//! gating here would lock ratatoskr out of the cascade it's
//! trying to exercise):
//!
//! - `GET /{prefix}/.well-known/webfinger?resource=...&rel=...`
//!   -> JRD per RFC 7033.
//! - `GET /{prefix}/.well-known/openid-configuration`
//!   -> OIDC discovery document per OpenID Connect Discovery 1.0.
//! - `GET /{prefix}/mail/config-v1.1.xml`
//!   -> Mozilla autoconfig XML (raw body, fixture-authored).
//!
//! `{prefix}` is whatever path segment(s) come before the
//! well-known suffix. Axum 0.8 only allows the `{*name}` wildcard
//! at the end of a route, so the three shapes share a single
//! `/{*discovery_path}` route that suffix-matches in the handler.
//! Unmatched suffixes return 404 (preserving the prior behaviour
//! for unknown paths).
//!
//! Lookup against `Fixture::discovery` is a flat string compare
//! on the captured prefix. No entry = 404. Path-relative URLs in
//! fixture docs (`/oauth/token`, `/idp/realms/...`) get the live
//! listener base URL spliced in at emit time; absolute URLs
//! (`http://insecure.example`) pass through verbatim so negative
//! tests can stage a non-HTTPS href.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fixture::{AutoconfigDoc, OidcDoc, WebFingerDoc, WebFingerLink};
use crate::request_log::RequestEntry;
use crate::routes::AppState;

const WEBFINGER_SUFFIX: &str = "/.well-known/webfinger";
const OIDC_SUFFIX: &str = "/.well-known/openid-configuration";
const AUTOCONFIG_SUFFIX: &str = "/mail/config-v1.1.xml";

#[derive(Debug, Default, Deserialize)]
pub struct WebFingerParams {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub rel: Option<String>,
}

/// Single GET handler for every `/{prefix}/<known-suffix>` shape.
/// Axum 0.8 only matches `{*name}` wildcards in terminal position;
/// the three discovery suffixes share one route and dispatch on
/// the captured path's tail.
pub async fn dispatch(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<WebFingerParams>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    // axum captures the path without a leading `/`. Reconstruct
    // the leading slash so the suffix match is unambiguous (the
    // empty-prefix edge case `/{suffix}` strips down to "").
    let with_lead = format!("/{path}");
    if let Some(prefix) = strip_suffix(&with_lead, WEBFINGER_SUFFIX) {
        log_request(&state, "webfinger", prefix, &params, connection_id);
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        let Some(entry) = fixture.discovery.lookup(prefix) else {
            return not_found(prefix);
        };
        let Some(doc) = entry.webfinger.as_ref() else {
            return not_found(prefix);
        };
        return serve_webfinger(doc, &params, &state.base_url, prefix);
    }
    if let Some(prefix) = strip_suffix(&with_lead, OIDC_SUFFIX) {
        log_request(
            &state,
            "openid-configuration",
            prefix,
            &params,
            connection_id,
        );
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        let Some(entry) = fixture.discovery.lookup(prefix) else {
            return not_found(prefix);
        };
        let Some(doc) = entry.oidc.as_ref() else {
            return not_found(prefix);
        };
        return serve_oidc(doc, &state.base_url);
    }
    if let Some(prefix) = strip_suffix(&with_lead, AUTOCONFIG_SUFFIX) {
        log_request(&state, "autoconfig", prefix, &params, connection_id);
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        let Some(entry) = fixture.discovery.lookup(prefix) else {
            return not_found_xml(prefix);
        };
        let Some(doc) = entry.autoconfig.as_ref() else {
            return not_found_xml(prefix);
        };
        return serve_autoconfig(doc, &state.base_url);
    }
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// Strip `suffix` from `s` and return the prefix without the
/// surrounding `/`. Returns None if `s` doesn't end with the
/// suffix or if there's nothing left after stripping.
fn strip_suffix<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let stripped = s.strip_suffix(suffix)?;
    // Drop the leading `/` we added in dispatch.
    let stripped = stripped.strip_prefix('/').unwrap_or(stripped);
    if stripped.is_empty() {
        return None;
    }
    Some(stripped)
}

fn log_request(
    state: &AppState,
    command: &'static str,
    prefix: &str,
    params: &WebFingerParams,
    connection_id: Option<u64>,
) {
    let mut detail = json!({ "prefix": prefix });
    if let Some(r) = &params.resource {
        detail["resource"] = json!(r);
    }
    if let Some(r) = &params.rel {
        detail["rel"] = json!(r);
    }
    state.shared.request_log.push(RequestEntry {
        protocol: "discovery",
        command: command.to_string(),
        received_at: Timestamp::now(),
        detail,
        connection_id,
    });
}

fn serve_webfinger(
    doc: &WebFingerDoc,
    params: &WebFingerParams,
    base_url: &str,
    prefix: &str,
) -> Response {
    if let Some(body) = &doc.raw_body {
        let ct = doc
            .raw_content_type
            .as_deref()
            .unwrap_or("application/jrd+json");
        return raw_body(StatusCode::OK, ct, body.clone());
    }
    let filtered: Vec<&WebFingerLink> = match &params.rel {
        Some(rel) => doc.links.iter().filter(|l| &l.rel == rel).collect(),
        None => doc.links.iter().collect(),
    };
    let subject = params
        .resource
        .clone()
        .unwrap_or_else(|| format!("acct:unknown@{prefix}"));
    let body = json!({
        "subject": subject,
        "links": filtered
            .iter()
            .map(|l| json!({ "rel": l.rel, "href": prefix_url(&l.href, base_url) }))
            .collect::<Vec<_>>(),
    });
    let ct = doc
        .raw_content_type
        .as_deref()
        .unwrap_or("application/jrd+json");
    let bytes = serde_json::to_vec(&body).expect("json serialisation cannot fail");
    raw_body_bytes(StatusCode::OK, ct, bytes)
}

fn serve_oidc(doc: &OidcDoc, base_url: &str) -> Response {
    if let Some(body) = &doc.raw_body {
        let ct = doc
            .raw_content_type
            .as_deref()
            .unwrap_or("application/json");
        return raw_body(StatusCode::OK, ct, body.clone());
    }
    let mut body = serde_json::Map::new();
    body.insert(
        "issuer".to_string(),
        Value::String(prefix_url(&doc.issuer, base_url)),
    );
    body.insert(
        "authorization_endpoint".to_string(),
        Value::String(prefix_url(&doc.authorization_endpoint, base_url)),
    );
    body.insert(
        "token_endpoint".to_string(),
        Value::String(prefix_url(&doc.token_endpoint, base_url)),
    );
    if let Some(ui) = &doc.userinfo_endpoint {
        body.insert(
            "userinfo_endpoint".to_string(),
            Value::String(prefix_url(ui, base_url)),
        );
    }
    body.insert(
        "scopes_supported".to_string(),
        Value::Array(
            doc.scopes_supported
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    body.insert(
        "code_challenge_methods_supported".to_string(),
        Value::Array(
            doc.code_challenge_methods_supported
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    body.insert(
        "token_endpoint_auth_methods_supported".to_string(),
        Value::Array(
            doc.token_endpoint_auth_methods_supported
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        ),
    );
    let ct = doc
        .raw_content_type
        .as_deref()
        .unwrap_or("application/json");
    let bytes = serde_json::to_vec(&Value::Object(body)).expect("json serialisation cannot fail");
    raw_body_bytes(StatusCode::OK, ct, bytes)
}

fn serve_autoconfig(doc: &AutoconfigDoc, base_url: &str) -> Response {
    let body = doc.raw_body.replace("${BASE}", base_url);
    let ct = doc.raw_content_type.as_deref().unwrap_or("application/xml");
    raw_body(StatusCode::OK, ct, body)
}

/// Splice `base_url` in front of a path-relative URL. Absolute
/// URLs (`http://...` / `https://...`) are returned verbatim so
/// negative tests can stage non-HTTPS hrefs.
fn prefix_url(url: &str, base_url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if let Some(stripped) = url.strip_prefix('/') {
        format!("{base_url}/{stripped}")
    } else {
        format!("{base_url}/{url}")
    }
}

fn raw_body(status: StatusCode, ct: &str, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, ct.to_string())],
        Body::from(body),
    )
        .into_response()
}

fn raw_body_bytes(status: StatusCode, ct: &str, body: Vec<u8>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, ct.to_string())],
        Body::from(body),
    )
        .into_response()
}

fn not_found(prefix: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "not_found",
            "detail": format!("no discovery entry for prefix {prefix:?}"),
        })),
    )
        .into_response()
}

fn not_found_xml(prefix: &str) -> Response {
    raw_body(
        StatusCode::NOT_FOUND,
        "application/xml",
        format!("<error>no discovery entry for prefix {prefix}</error>"),
    )
}
