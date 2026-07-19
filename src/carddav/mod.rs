//! CardDAV contact mock listener.
//!
//! Serves the WebDAV / CardDAV verb surface bifrost-carddav
//! (`<bifrost>/crates/carddav/src/`) drives: the PROPFIND discovery
//! chain (current-user-principal -> addressbook-home-set -> Depth:1
//! addressbook collection listing carrying a CS:getctag), the
//! `REPORT addressbook-multiget` returning raw vCard bodies, the
//! Depth:0 ctag short-circuit, `REPORT addressbook-query` text-match
//! search, and the write verbs (PUT with If-None-Match create /
//! If-Match update, DELETE).
//!
//! Structure mirrors `src/caldav/`: its own port (CardDAV uses the
//! non-standard PROPFIND / REPORT verbs, and clients identify the
//! listener by its DAV-namespaced response shape), a single `any`
//! fallback dispatching on `(method, path)`, and the shared XML
//! helpers reused from `crate::caldav::xml`.
//!
//! Address books project from the fixture's `[[contact_folder]]`
//! entries and vCard contacts from `[[contact]]`, so a single fixture
//! exercises the CardDAV, Graph, JMAP, and People contact surfaces at
//! once. Mutations (`PUT` / `DELETE`) route through `Fixture::mutate`
//! so the change_log lights up the same `contact_*` id sets Graph
//! `contacts/delta`, JMAP `ContactCard/changes`, and the People
//! listener observe.
//!
//! A contact flagged `malformed_vcard` serves a body bifrost cannot
//! tokenize (`vcard::contact_to_vcard`), so bifrost fetches it, fails
//! to parse it, and records its id in `Page::failed_ids` rather than
//! treating the absence as a deletion.

pub mod vcard;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::caldav::xml;
use crate::caldav::{bad_request, body_to_str, depth_header, multistatus};
use crate::oauth::BearerDecision;
use crate::shared::SharedHandles;

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedHandles,
}

impl AppState {
    pub fn for_test(fixture: crate::shared::FixtureHandle) -> Self {
        Self {
            shared: SharedHandles::for_test(fixture),
        }
    }
}

/// Build the CardDAV router. Same single-`any`-fallback shape as
/// CalDAV since PROPFIND / REPORT are not enumerable via axum's
/// `MethodFilter`; bearer enforcement layers in front when the
/// fixture flips `[oauth] enforce`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(dispatch))
        .route("/{*rest}", any(dispatch))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_bearer_middleware,
        ))
        .with_state(state)
}

async fn enforce_bearer_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let decision = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        crate::oauth::check_bearer(&fixture, &state.shared.token_store, req.headers())
    };
    match decision {
        BearerDecision::Allow => next.run(req).await,
        BearerDecision::Deny(_reason) => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", "Bearer")
            .body(Body::empty())
            .expect("static 401 response builds"),
    }
}

pub async fn serve(
    listener: TcpListener,
    state: AppState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::connection_id::ConnInfo>(),
    )
    .with_graceful_shutdown(async move {
        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                return;
            }
        }
    })
    .await
}

const PROPFIND: &str = "PROPFIND";
const REPORT: &str = "REPORT";

/// Resolve the requesting principal from an `Authorization: Basic`
/// header, else `None`. Mirrors CalDAV's principal resolution so a
/// multi-account fixture routes the bootstrap PROPFIND on `/` to the
/// authenticating principal.
fn account_from_basic_auth(
    fixture: &crate::fixture::Fixture,
    headers: &HeaderMap,
) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let b64 = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = crate::imap::sasl_decode_b64(b64.trim())?;
    let creds = std::str::from_utf8(&decoded).ok()?;
    let (user, _pass) = creds.split_once(':')?;
    fixture.account(user).map(|a| a.id.clone())
}

async fn dispatch(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    state.shared.request_log.record_with_conn(
        "carddav",
        format!("{method} {path}"),
        json!({ "query": uri.query() }),
        connection_id,
    );
    state.shared.latency.sleep_for("carddav").await;

    let m = method.as_str();
    if m == PROPFIND {
        return handle_propfind(&state, &path, &headers, &body).await;
    }
    if m == REPORT {
        return handle_report(&state, &path, &body).await;
    }
    match method {
        Method::GET => handle_get(&state, &path).await,
        Method::PUT => handle_put(&state, &path, &headers, &body).await,
        Method::DELETE => handle_delete(&state, &path, &headers).await,
        Method::OPTIONS => handle_options(),
        _ => not_found(&format!("{method} {path}")),
    }
}

// ── Path parsing ────────────────────────────────────────────────────

#[derive(Debug)]
enum ResourcePath {
    Root,
    WellKnown,
    Principal {
        user: String,
    },
    AddressBookHome {
        user: String,
    },
    AddressBook {
        user: String,
        book: String,
    },
    Contact {
        user: String,
        book: String,
        contact_id: String,
    },
    Unknown,
}

fn parse_path(fixture: &crate::fixture::Fixture, path: &str) -> ResourcePath {
    if path == "/" {
        return ResourcePath::Root;
    }
    if path == "/.well-known/carddav" || path == "/.well-known/carddav/" {
        return ResourcePath::WellKnown;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.split('/').skip(1).any(str::is_empty) {
        return ResourcePath::Unknown;
    }
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["principals", u] if fixture.account(u).is_some() => ResourcePath::Principal {
            user: (*u).to_string(),
        },
        ["addressbooks", u] if fixture.account(u).is_some() => ResourcePath::AddressBookHome {
            user: (*u).to_string(),
        },
        ["addressbooks", u, book] if fixture.account(u).is_some() => ResourcePath::AddressBook {
            user: (*u).to_string(),
            book: (*book).to_string(),
        },
        ["addressbooks", u, book, resource] if fixture.account(u).is_some() => {
            let contact_id = resource
                .strip_suffix(".vcf")
                .unwrap_or(resource)
                .to_string();
            ResourcePath::Contact {
                user: (*u).to_string(),
                book: (*book).to_string(),
                contact_id,
            }
        }
        _ => ResourcePath::Unknown,
    }
}

// ── PROPFIND ────────────────────────────────────────────────────────

async fn handle_propfind(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let depth = depth_header(headers);
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    let auth_user = account_from_basic_auth(&fixture, headers);
    match parse_path(&fixture, path) {
        ResourcePath::Root | ResourcePath::WellKnown => {
            propfind_root(&fixture, auth_user.as_deref(), body_str)
        }
        ResourcePath::Principal { user } => propfind_principal(&fixture, &user, body_str),
        ResourcePath::AddressBookHome { user } => propfind_home(&fixture, &user, body_str, depth),
        ResourcePath::AddressBook { user, book } => {
            propfind_addressbook(&fixture, &user, &book, body_str, depth)
        }
        ResourcePath::Contact {
            user,
            book,
            contact_id,
        } => propfind_contact(&fixture, &user, &book, &contact_id, body_str),
        ResourcePath::Unknown => not_found(path),
    }
}

fn propfind_root(
    fixture: &crate::fixture::Fixture,
    auth_user: Option<&str>,
    body: &str,
) -> Response {
    let requested = xml::requested_props(body);
    let primary_id = fixture.primary_account().id.clone();
    let user = auth_user.unwrap_or(&primary_id);
    let principal_url = format!("/principals/{user}/");
    let mut props = String::new();
    if requested.contains("current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
            xml::escape(&principal_url),
        ));
    }
    if requested.contains("principal-URL") {
        props.push_str(&format!(
            "<D:principal-URL><D:href>{}</D:href></D:principal-URL>",
            xml::escape(&principal_url),
        ));
    }
    multistatus(wrap_responses(&[Response207 {
        href: "/",
        ok_props: &props,
    }]))
}

fn propfind_principal(fixture: &crate::fixture::Fixture, user: &str, body: &str) -> Response {
    let requested = xml::requested_props(body);
    let home_url = format!("/addressbooks/{user}/");
    let mut props = String::new();
    if requested.contains("addressbook-home-set") {
        props.push_str(&format!(
            "<C:addressbook-home-set><D:href>{}</D:href></C:addressbook-home-set>",
            xml::escape(&home_url),
        ));
    }
    if requested.contains("current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>/principals/{}/</D:href></D:current-user-principal>",
            xml::escape(user),
        ));
    }
    if requested.contains("displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(&fixture.primary_account().name),
        ));
    }
    multistatus(wrap_responses(&[Response207 {
        href: &format!("/principals/{user}/"),
        ok_props: &props,
    }]))
}

/// PROPFIND on `/addressbooks/{user}/`. Depth=0 returns just the home
/// collection; Depth=1 lists every address book (the account's contact
/// folders) plus the home. Each book carries its resourcetype
/// (collection + addressbook), displayname, and CS:getctag.
fn propfind_home(fixture: &crate::fixture::Fixture, user: &str, body: &str, depth: u8) -> Response {
    let requested = xml::requested_props(body);
    let home_href = format!("/addressbooks/{user}/");
    let home_props = home_collection_props(&requested);
    let mut entries = vec![Response207 {
        href: &home_href,
        ok_props: &home_props,
    }];
    let mut book_hrefs = Vec::new();
    let mut book_props = Vec::new();
    if depth >= 1 {
        for folder in fixture.contact_folders_for(user) {
            book_hrefs.push(format!("/addressbooks/{user}/{}/", folder.id));
            book_props.push(addressbook_props(
                fixture,
                &folder.id,
                &folder.display_name,
                &requested,
            ));
        }
    }
    for (i, href) in book_hrefs.iter().enumerate() {
        entries.push(Response207 {
            href,
            ok_props: &book_props[i],
        });
    }
    multistatus(wrap_responses(&entries))
}

/// PROPFIND on `/addressbooks/{user}/{book}/`. Depth=0 returns the
/// book's own props (including CS:getctag for the short-circuit);
/// Depth=1 also lists each vCard resource (getetag + getcontenttype).
fn propfind_addressbook(
    fixture: &crate::fixture::Fixture,
    user: &str,
    book: &str,
    body: &str,
    depth: u8,
) -> Response {
    let requested = xml::requested_props(body);
    let folder = match fixture.contact_folders_for(user).find(|f| f.id == book) {
        Some(f) => f,
        None => return not_found(&format!("/addressbooks/{user}/{book}/")),
    };
    let book_href = format!("/addressbooks/{user}/{book}/");
    let book_props = addressbook_props(fixture, &folder.id, &folder.display_name, &requested);
    let mut entries = vec![Response207 {
        href: &book_href,
        ok_props: &book_props,
    }];
    let mut contact_hrefs = Vec::new();
    let mut contact_props = Vec::new();
    if depth >= 1 {
        for c in fixture.contacts_for(user).filter(|c| c.folder_id == book) {
            contact_hrefs.push(format!("/addressbooks/{user}/{book}/{}.vcf", c.id));
            contact_props.push(contact_resource_props(fixture, &c.id, &requested));
        }
    }
    for (i, href) in contact_hrefs.iter().enumerate() {
        entries.push(Response207 {
            href,
            ok_props: &contact_props[i],
        });
    }
    multistatus(wrap_responses(&entries))
}

fn propfind_contact(
    fixture: &crate::fixture::Fixture,
    user: &str,
    book: &str,
    contact_id: &str,
    body: &str,
) -> Response {
    let requested = xml::requested_props(body);
    let exists = fixture
        .contacts_for(user)
        .any(|c| c.id == contact_id && c.folder_id == book);
    if !exists {
        return not_found(&format!("/addressbooks/{user}/{book}/{contact_id}.vcf"));
    }
    let href = format!("/addressbooks/{user}/{book}/{contact_id}.vcf");
    let props = contact_resource_props(fixture, contact_id, &requested);
    multistatus(wrap_responses(&[Response207 {
        href: &href,
        ok_props: &props,
    }]))
}

// ── Property serialization ──────────────────────────────────────────

fn home_collection_props(requested: &std::collections::HashSet<String>) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    }
    if requested.contains("displayname") {
        props.push_str("<D:displayname>Address Books</D:displayname>");
    }
    props
}

fn addressbook_props(
    fixture: &crate::fixture::Fixture,
    book: &str,
    display_name: &str,
    requested: &std::collections::HashSet<String>,
) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        props.push_str("<D:resourcetype><D:collection/><C:addressbook/></D:resourcetype>");
    }
    if requested.contains("displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(display_name),
        ));
    }
    if requested.contains("getctag") {
        props.push_str(&format!(
            "<CS:getctag>{}</CS:getctag>",
            xml::escape(&addressbook_ctag(fixture, book)),
        ));
    }
    props
}

fn contact_resource_props(
    fixture: &crate::fixture::Fixture,
    contact_id: &str,
    requested: &std::collections::HashSet<String>,
) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        props.push_str("<D:resourcetype/>");
    }
    if requested.contains("getetag") {
        props.push_str(&format!(
            "<D:getetag>{}</D:getetag>",
            xml::escape(&contact_etag(fixture, contact_id)),
        ));
    }
    if requested.contains("getcontenttype") {
        props.push_str("<D:getcontenttype>text/vcard; charset=utf-8</D:getcontenttype>");
    }
    props
}

struct Response207<'a> {
    href: &'a str,
    ok_props: &'a str,
}

fn wrap_responses(entries: &[Response207<'_>]) -> String {
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push('\n');
    out.push_str(
        r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav" xmlns:CS="http://calendarserver.org/ns/">"#,
    );
    for e in entries {
        out.push_str("<D:response>");
        out.push_str(&format!("<D:href>{}</D:href>", xml::escape(e.href)));
        out.push_str("<D:propstat>");
        out.push_str("<D:prop>");
        out.push_str(e.ok_props);
        out.push_str("</D:prop>");
        out.push_str("<D:status>HTTP/1.1 200 OK</D:status>");
        out.push_str("</D:propstat>");
        out.push_str("</D:response>");
    }
    out.push_str("</D:multistatus>");
    out
}

// ── CTag / ETag (change_log-derived, deterministic) ─────────────────

/// Deterministic CTag for an address book: the most recent change_log
/// state that touched any contact in the folder, so the ctag advances
/// only when the book's contents actually change (mirrors CalDAV's
/// calendar ctag). Feeds bifrost's Depth:0 short-circuit.
fn addressbook_ctag(fixture: &crate::fixture::Fixture, book: &str) -> String {
    let last = last_state_touching_addressbook(fixture, book);
    format!("{last}/{book}")
}

/// Deterministic ETag for a contact resource: the most recent
/// change_log state that touched the contact. bifrost only checks
/// byte-equality, so the format is free to evolve.
fn contact_etag(fixture: &crate::fixture::Fixture, contact_id: &str) -> String {
    let last = last_state_touching_contact(fixture, contact_id);
    format!("\"{last}/{contact_id}\"")
}

fn last_state_touching_contact(fixture: &crate::fixture::Fixture, contact_id: &str) -> String {
    let Some(account_id) = fixture
        .contacts
        .iter()
        .find(|c| c.id == contact_id)
        .map(|c| c.account_id.clone())
    else {
        return fixture.change_log_seed().to_string();
    };
    for t in fixture.change_log_transitions_for(&account_id).rev() {
        if t.contact_created.iter().any(|id| id == contact_id)
            || t.contact_updated.iter().any(|id| id == contact_id)
            || t.contact_destroyed.iter().any(|id| id == contact_id)
        {
            return t.to_state.clone();
        }
    }
    fixture.change_log_seed().to_string()
}

fn last_state_touching_addressbook(fixture: &crate::fixture::Fixture, book: &str) -> String {
    let Some(account_id) = fixture
        .contact_folders
        .iter()
        .find(|f| f.id == book)
        .map(|f| f.account_id.clone())
    else {
        return fixture.change_log_seed().to_string();
    };
    for t in fixture.change_log_transitions_for(&account_id).rev() {
        let touches_live = t
            .contact_created
            .iter()
            .chain(t.contact_updated.iter())
            .any(|id| {
                fixture
                    .contacts
                    .iter()
                    .any(|c| &c.id == id && c.folder_id == book)
            });
        if touches_live {
            return t.to_state.clone();
        }
        if t.contact_destroyed_parents.iter().any(|p| p == book) {
            return t.to_state.clone();
        }
    }
    fixture.change_log_seed().to_string()
}

// ── REPORT ──────────────────────────────────────────────────────────

async fn handle_report(state: &AppState, path: &str, body: &[u8]) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    let (user, book) = match parse_path(&fixture, path) {
        ResourcePath::AddressBook { user, book } => (user, book),
        _ => return not_found(path),
    };
    if !fixture.contact_folders_for(&user).any(|f| f.id == book) {
        return not_found(path);
    }
    if xml::body_requests_prop(body_str, "addressbook-multiget") {
        return report_multiget(&fixture, &user, &book, body_str);
    }
    if xml::body_requests_prop(body_str, "addressbook-query") {
        return report_query(&fixture, &user, &book, body_str);
    }
    bad_request("REPORT body must be addressbook-multiget or addressbook-query")
}

/// `addressbook-multiget`: the client lists `<href>` elements; we
/// return one `<response>` per href carrying getetag + `<C:address-data>`
/// (the raw vCard body). Hrefs not resolving to a contact in this book
/// get a 404 propstat.
fn report_multiget(
    fixture: &crate::fixture::Fixture,
    user: &str,
    book: &str,
    body: &str,
) -> Response {
    let hrefs = xml::collect_hrefs(body);
    let mut out = report_prelude();
    for href in hrefs {
        let contact_id = match parse_path(fixture, &href) {
            ResourcePath::Contact {
                user: u,
                book: b,
                contact_id,
            } if u == user && b == book => Some(contact_id),
            _ => None,
        };
        let contact = contact_id.as_deref().and_then(|id| {
            fixture
                .contacts_for(user)
                .find(|c| c.id == id && c.folder_id == book)
        });
        match contact {
            Some(c) => push_card_response(
                &mut out,
                &href,
                &contact_etag(fixture, &c.id),
                &vcard::contact_to_vcard(c),
            ),
            None => {
                out.push_str("<D:response>");
                out.push_str(&format!("<D:href>{}</D:href>", xml::escape(&href)));
                out.push_str("<D:status>HTTP/1.1 404 Not Found</D:status>");
                out.push_str("</D:response>");
            }
        }
    }
    out.push_str("</D:multistatus>");
    multistatus(out)
}

/// `addressbook-query`: bifrost issues one text-match `prop-filter` per
/// vCard property (FN / N / EMAIL / TEL / ADR / ORG / TITLE / NOTE),
/// deduping results client-side. We honour the `<C:prop-filter
/// name="X">` + `<C:text-match>` shape by matching the named property's
/// value against the text substring (case-insensitive).
fn report_query(fixture: &crate::fixture::Fixture, user: &str, book: &str, body: &str) -> Response {
    let filter = parse_query_filter(body);
    let mut out = report_prelude();
    for c in fixture.contacts_for(user).filter(|c| c.folder_id == book) {
        if let Some((property, needle)) = &filter
            && !contact_property_matches(c, property, needle)
        {
            continue;
        }
        let href = format!("/addressbooks/{user}/{book}/{}.vcf", c.id);
        push_card_response(
            &mut out,
            &href,
            &contact_etag(fixture, &c.id),
            &vcard::contact_to_vcard(c),
        );
    }
    out.push_str("</D:multistatus>");
    multistatus(out)
}

fn report_prelude() -> String {
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push('\n');
    out.push_str(r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav">"#);
    out
}

fn push_card_response(out: &mut String, href: &str, etag: &str, vcard_body: &str) {
    out.push_str("<D:response>");
    out.push_str(&format!("<D:href>{}</D:href>", xml::escape(href)));
    out.push_str("<D:propstat><D:prop>");
    out.push_str(&format!("<D:getetag>{}</D:getetag>", xml::escape(etag)));
    out.push_str(&format!(
        "<C:address-data>{}</C:address-data>",
        xml::escape(vcard_body),
    ));
    out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
    out.push_str("</D:response>");
}

/// Parse the `<C:prop-filter name="X"><C:text-match>needle` shape into
/// `(property, needle)`. Returns `None` when absent (match-all).
fn parse_query_filter(body: &str) -> Option<(String, String)> {
    let idx = body.find("prop-filter")?;
    let tail = &body[idx..];
    let name_key = "name=\"";
    let name_start = tail.find(name_key)? + name_key.len();
    let name_end = tail[name_start..].find('"')?;
    let property = tail[name_start..name_start + name_end].to_ascii_uppercase();
    // Text between the text-match open tag and its close.
    let tm = tail.find("text-match")?;
    let after = &tail[tm..];
    let gt = after.find('>')?;
    let value_start = gt + 1;
    let value_slice = &after[value_start..];
    let close = value_slice.find("</")?;
    let needle = value_slice[..close].trim().to_string();
    if needle.is_empty() {
        return None;
    }
    Some((property, needle))
}

fn contact_property_matches(c: &crate::fixture::Contact, property: &str, needle: &str) -> bool {
    let n = needle.to_lowercase();
    let has = |s: &str| s.to_lowercase().contains(&n);
    match property {
        "FN" | "N" => c.display_name.as_deref().is_some_and(has),
        "EMAIL" => c.emails.iter().any(|e| has(&e.address)),
        "TEL" => c.phones.iter().any(|p| has(&p.number)),
        "ORG" => c.company.as_deref().is_some_and(has),
        "TITLE" => c.job_title.as_deref().is_some_and(has),
        "NOTE" => c.notes.as_deref().is_some_and(has),
        // ADR and any other property: no dedicated fixture field.
        _ => false,
    }
}

// ── GET / PUT / DELETE ──────────────────────────────────────────────

async fn handle_get(state: &AppState, path: &str) -> Response {
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    match parse_path(&fixture, path) {
        ResourcePath::Contact {
            user,
            book,
            contact_id,
        } => match fixture
            .contacts_for(&user)
            .find(|c| c.id == contact_id && c.folder_id == book)
        {
            Some(c) => {
                let body = vcard::contact_to_vcard(c);
                let etag = contact_etag(&fixture, &c.id);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("text/vcard; charset=utf-8"),
                    )
                    .header("ETag", etag)
                    .body(Body::from(body))
                    .expect("static GET response builds")
            }
            None => not_found(path),
        },
        _ => not_found(path),
    }
}

async fn handle_put(state: &AppState, path: &str, headers: &HeaderMap, body: &[u8]) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let if_none_match = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);

    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    let (user, book, contact_id) = match parse_path(&fixture, path) {
        ResourcePath::Contact {
            user,
            book,
            contact_id,
        } => (user, book, contact_id),
        _ => return not_found(path),
    };
    // The folder must exist and belong to this principal.
    let folder = match fixture.contact_folders_for(&user).find(|f| f.id == book) {
        Some(f) => f.clone(),
        None => return not_found(path),
    };

    let existing_idx = fixture
        .contacts
        .iter()
        .position(|c| c.id == contact_id && c.folder_id == book);

    // Preconditions. If-None-Match:* means "create only"; If-Match
    // means "update only, matching etag".
    if let Some(inm) = if_none_match
        && inm.trim() == "*"
        && existing_idx.is_some()
    {
        return precondition_failed("If-None-Match: * but resource exists");
    }
    if let Some(im) = if_match {
        match existing_idx {
            Some(idx) => {
                let current = contact_etag(&fixture, &fixture.contacts[idx].id);
                if !if_match_matches(im, &current) {
                    return precondition_failed("If-Match did not match current ETag");
                }
            }
            None => return precondition_failed("If-Match but resource does not exist"),
        }
    }

    let parsed = vcard::parse_vcard(body_str);
    let display_name = parsed.display_name;
    let emails = parsed.emails;
    let phones = parsed.phones;
    let company = parsed.company;
    let job_title = parsed.job_title;
    let notes = parsed.notes;

    let was_create = existing_idx.is_none();
    let new_contact = crate::fixture::Contact {
        id: contact_id.clone(),
        account_id: folder.account_id.clone(),
        folder_id: book.clone(),
        display_name,
        emails,
        phones,
        company,
        job_title,
        // Department has no vCard slot in the mock's projection; a PUT
        // does not carry it, so it is cleared / left unset.
        department: existing_idx.and_then(|i| fixture.contacts[i].department.clone()),
        notes,
        groups: existing_idx
            .map(|i| fixture.contacts[i].groups.clone())
            .unwrap_or_default(),
        malformed_vcard: false,
    };

    let id_for_diff = contact_id.clone();
    fixture.mutate(|f| match existing_idx {
        Some(idx) => {
            f.contacts[idx] = new_contact.clone();
            crate::fixture::MutationDiff {
                contact_updated: vec![id_for_diff.clone()],
                ..Default::default()
            }
        }
        None => {
            f.contacts.push(new_contact.clone());
            crate::fixture::MutationDiff {
                contact_created: vec![id_for_diff.clone()],
                ..Default::default()
            }
        }
    });
    let etag = contact_etag(&fixture, &contact_id);
    let status = if was_create {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    Response::builder()
        .status(status)
        .header("ETag", etag)
        .body(Body::empty())
        .expect("static PUT response builds")
}

async fn handle_delete(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    let (user, book, contact_id) = match parse_path(&fixture, path) {
        ResourcePath::Contact {
            user,
            book,
            contact_id,
        } => (user, book, contact_id),
        _ => return not_found(path),
    };
    if !fixture.contact_folders_for(&user).any(|f| f.id == book) {
        return not_found(path);
    }
    let idx = fixture
        .contacts
        .iter()
        .position(|c| c.id == contact_id && c.folder_id == book && c.account_id == user);
    let Some(idx) = idx else {
        return not_found(path);
    };
    if let Some(im) = if_match {
        let current = contact_etag(&fixture, &fixture.contacts[idx].id);
        if !if_match_matches(im, &current) {
            return precondition_failed("If-Match did not match current ETag");
        }
    }
    let id = fixture.contacts[idx].id.clone();
    let parent = fixture.contacts[idx].folder_id.clone();
    fixture.mutate(|f| {
        f.contacts.remove(idx);
        crate::fixture::MutationDiff {
            contact_destroyed: vec![id.clone()],
            contact_destroyed_parents: vec![parent.clone()],
            ..Default::default()
        }
    });
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static DELETE response builds")
}

/// RFC 7232 If-Match against the current ETag. Tolerates the `W/`
/// weak prefix, surrounding quotes, comma lists, and the wildcard.
fn if_match_matches(header_value: &str, current_etag: &str) -> bool {
    let cur = unquote(current_etag);
    for tag in header_value.split(',') {
        let trimmed = tag.trim();
        let unweak = trimmed.strip_prefix("W/").unwrap_or(trimmed).trim();
        let unquoted = unquote(unweak);
        if unquoted == "*" || unquoted == cur {
            return true;
        }
    }
    false
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

fn handle_options() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("DAV", "1, 3, addressbook")
        .header("Allow", "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE")
        .body(Body::empty())
        .expect("static OPTIONS response builds")
}

fn precondition_failed(msg: &str) -> Response {
    (StatusCode::PRECONDITION_FAILED, msg.to_string()).into_response()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("not found: {what}")).into_response()
}
