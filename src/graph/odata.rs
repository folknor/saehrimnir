//! OData query plumbing shared across Graph resources.
//!
//! v0 supports the system query options ratatoskr's mail-sync code
//! actually emits: `$top`, `$skip`, `$filter`, `$select`, `$expand`,
//! `$orderby`, `$count`. Everything else is parsed as a free-form
//! string and ignored.
//!
//! Pagination cursors travel as opaque strings in `$skiptoken` /
//! `$deltatoken`. The wire spec says they are server-defined; the
//! client never inspects them. v0 uses `s.<n>` for skiptokens and
//! `d.<n>` for deltatokens. The format is intentionally short and
//! URL-safe so no encoding is needed.

use serde_json::{Value, json};

/// Parsed OData query parameters. All fields are optional; missing
/// values use the resource's defaults.
#[derive(Debug, Default)]
pub struct OdataQuery {
    pub top: Option<u32>,
    pub skip: Option<u32>,
    pub filter: Option<String>,
    pub select: Option<String>,
    pub expand: Option<String>,
    pub orderby: Option<String>,
    pub count: Option<bool>,
    pub skiptoken: Option<String>,
    pub deltatoken: Option<String>,
}

impl OdataQuery {
    /// Build from an axum `RawQuery` (which is the raw `?...` minus
    /// the leading `?`). Empty input yields an all-default
    /// `OdataQuery`.
    pub fn parse(raw: Option<&str>) -> Self {
        let mut out = Self::default();
        let Some(s) = raw else { return out };
        let pairs = parse_query_pairs(s);
        for (k, v) in pairs {
            match k.as_str() {
                "$top" => out.top = v.parse().ok(),
                "$skip" => out.skip = v.parse().ok(),
                "$filter" => out.filter = Some(v),
                "$select" => out.select = Some(v),
                "$expand" => out.expand = Some(v),
                "$orderby" => out.orderby = Some(v),
                "$count" => out.count = Some(v == "true"),
                "$skiptoken" => out.skiptoken = Some(v),
                "$deltatoken" => out.deltatoken = Some(v),
                _ => {}
            }
        }
        out
    }

    /// Effective offset for paged collections. Returns `None` if the
    /// client supplied a `$skiptoken` we can't decode - that's a
    /// pagination contract violation that should surface as a
    /// `400 InvalidQueryParameter`, not a silent restart from offset
    /// 0 (which would loop ratatoskr forever on a stale token).
    /// Falls back to `$skip` (default 0) when no `$skiptoken` is
    /// present.
    pub fn offset(&self) -> Option<u32> {
        if let Some(t) = self.skiptoken.as_deref() {
            return decode_skiptoken(t);
        }
        Some(self.skip.unwrap_or(0))
    }

    /// Resolve `$top` against a resource-specific default and a
    /// resource-specific maximum. The Graph API caps page sizes
    /// implicitly; we apply the same cap up front so an over-eager
    /// client can't ask for the moon.
    pub fn page_size(&self, default: u32, max: u32) -> u32 {
        self.top.unwrap_or(default).min(max).max(1)
    }
}

/// Look up a single non-`$` query parameter, url-decoded. Used for
/// the calendar `calendarView` range params (`startDateTime` /
/// `endDateTime`), which sit outside the OData `$` set `OdataQuery`
/// captures.
pub fn query_param(raw: Option<&str>, key: &str) -> Option<String> {
    parse_query_pairs(raw?)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// URL-decode a query string into `(key, value)` pairs without
/// pulling in `url::form_urlencoded` for one call site.
fn parse_query_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = hex(bytes[i + 1]);
                let lo = hex(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-decode one URL *path* segment.
///
/// Distinct from the query decoder above in one way that matters: a
/// literal `+` stays a `+` rather than becoming a space. `+` is a
/// perfectly ordinary path character, and it shows up verbatim inside
/// base64-shaped Microsoft Graph message ids; a client that means a
/// space in a path segment sends `%20`.
///
/// Malformed escapes (a trailing `%`, `%X`, non-hex digits) pass
/// through verbatim so a deliberately-broken URL still routes
/// deterministically instead of losing bytes.
///
/// Callers must split the path on its raw `/` separators BEFORE
/// decoding each segment: an id carrying an encoded `%2F` would
/// otherwise decode into a segment boundary and be mis-read as
/// `{id}/{suffix}`.
pub fn decode_path_segment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode a numeric offset as a `$skiptoken` value. The format is
/// server-defined; clients treat tokens as opaque (`sync/folders.rs:200`).
pub fn encode_skiptoken(offset: u32) -> String {
    format!("s.{offset}")
}

fn decode_skiptoken(token: &str) -> Option<u32> {
    token.strip_prefix("s.")?.parse().ok()
}

/// Encode a delta-state cursor. The cursor wraps the fixture's
/// current state token (`Fixture::state`) so a follow-up call can
/// reconstruct the from-state for a delta walk. The fixture state
/// strings we emit are alphanumeric + `.` only (`<seed>` or
/// `<seed>.<n>`), so no URL-escaping is required for the values
/// we actually mint.
pub fn encode_deltatoken(state: &str) -> String {
    format!("d.{state}")
}

/// Inverse of [`encode_deltatoken`]. Returns the raw fixture-state
/// string the caller previously serialised. Returns `None` when the
/// token does not carry the `d.` prefix (treat as unknown -> full
/// re-bootstrap on Graph, `cannotCalculateChanges` on JMAP).
pub fn decode_deltatoken(token: &str) -> Option<&str> {
    token.strip_prefix("d.")
}

/// Build an `@odata.nextLink` absolute URL by re-using the request's
/// path and replacing/inserting `$skiptoken` and `$skip`.
pub fn build_next_link(host: &str, path: &str, raw_query: Option<&str>, offset: u32) -> String {
    let mut pairs = match raw_query {
        Some(s) => parse_query_pairs(s),
        None => Vec::new(),
    };
    pairs.retain(|(k, _)| k != "$skip" && k != "$skiptoken");
    pairs.push(("$skiptoken".to_string(), encode_skiptoken(offset)));
    let qs = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_form(k), encode_form(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("http://{host}{path}?{qs}")
}

/// Build an `@odata.deltaLink` absolute URL. The deltaLink is the
/// resource path with `$deltatoken` set; `$skiptoken` and `$skip`
/// are stripped because the deltaLink stands on its own for the
/// next cycle.
pub fn build_delta_link(host: &str, path: &str, raw_query: Option<&str>, state: &str) -> String {
    let mut pairs = match raw_query {
        Some(s) => parse_query_pairs(s),
        None => Vec::new(),
    };
    pairs.retain(|(k, _)| k != "$skip" && k != "$skiptoken" && k != "$deltatoken");
    pairs.push(("$deltatoken".to_string(), encode_deltatoken(state)));
    let qs = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_form(k), encode_form(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("http://{host}{path}?{qs}")
}

/// Wrap a slice of items in the canonical OData collection envelope.
/// `next_link` and `delta_link` are added when present.
pub fn collection_envelope(
    context: &str,
    value: Vec<Value>,
    next_link: Option<String>,
    delta_link: Option<String>,
    total_count: Option<u64>,
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "@odata.context".to_string(),
        Value::String(context.to_string()),
    );
    if let Some(c) = total_count {
        out.insert("@odata.count".to_string(), json!(c));
    }
    out.insert("value".to_string(), Value::Array(value));
    if let Some(n) = next_link {
        out.insert("@odata.nextLink".to_string(), Value::String(n));
    }
    if let Some(d) = delta_link {
        out.insert("@odata.deltaLink".to_string(), Value::String(d));
    }
    Value::Object(out)
}

/// Re-encode a query parameter component. `$` is left unencoded so
/// OData system query options (`$top`, `$skiptoken`, etc.) round-trip
/// in their canonical form.
fn encode_form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'$' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_and_skip() {
        let q = OdataQuery::parse(Some("$top=10&$skip=5"));
        assert_eq!(q.top, Some(10));
        assert_eq!(q.skip, Some(5));
    }

    #[test]
    fn parses_filter_with_url_encoded_value() {
        let q = OdataQuery::parse(Some(
            "$filter=receivedDateTime%20ge%202025-02-01T00%3A00%3A00Z",
        ));
        assert_eq!(
            q.filter.as_deref(),
            Some("receivedDateTime ge 2025-02-01T00:00:00Z")
        );
    }

    #[test]
    fn skiptoken_round_trips() {
        let token = encode_skiptoken(42);
        let q = OdataQuery::parse(Some(&format!("$skiptoken={token}")));
        assert_eq!(q.offset(), Some(42));
    }

    #[test]
    fn skiptoken_overrides_skip() {
        let token = encode_skiptoken(99);
        let q = OdataQuery::parse(Some(&format!("$skip=5&$skiptoken={token}")));
        assert_eq!(q.offset(), Some(99));
    }

    #[test]
    fn malformed_skiptoken_returns_none() {
        // Bug fix: previously a bad skiptoken silently fell back to
        // offset 0 (or $skip), which would restart pagination from
        // the beginning and loop the client forever.
        let q = OdataQuery::parse(Some("$skiptoken=garbage"));
        assert_eq!(q.offset(), None);
        // Even when $skip is also present, an unparseable skiptoken
        // still wins (the token is the contract; if we can't
        // decode it, that's a hard error).
        let q = OdataQuery::parse(Some("$skip=5&$skiptoken=garbage"));
        assert_eq!(q.offset(), None);
    }

    #[test]
    fn no_skiptoken_falls_back_to_skip() {
        let q = OdataQuery::parse(Some("$skip=5"));
        assert_eq!(q.offset(), Some(5));
        let q = OdataQuery::parse(None);
        assert_eq!(q.offset(), Some(0));
    }

    #[test]
    fn page_size_clamped_to_max() {
        let q = OdataQuery::parse(Some("$top=10000"));
        assert_eq!(q.page_size(50, 250), 250);
        let q = OdataQuery::parse(Some("$top=0"));
        assert_eq!(q.page_size(50, 250), 1);
    }

    #[test]
    fn next_link_includes_skiptoken_and_strips_prior_skip() {
        let nl = build_next_link(
            "127.0.0.1:1234",
            "/v1.0/me/messages",
            Some("$top=2&$skip=4"),
            6,
        );
        assert!(nl.starts_with("http://127.0.0.1:1234/v1.0/me/messages?"));
        assert!(nl.contains("$top=2"));
        assert!(nl.contains("$skiptoken="));
        assert!(!nl.contains("$skip=4"));
    }

    #[test]
    fn delta_link_strips_skip_and_skiptoken() {
        let dl = build_delta_link(
            "h",
            "/v1.0/me/mailFolders/x/messages/delta",
            Some("$skip=10&$skiptoken=abc&$top=50"),
            "fixture-state",
        );
        assert!(dl.contains("$top=50"));
        assert!(dl.contains("$deltatoken="));
        assert!(!dl.contains("$skip=10"));
        assert!(!dl.contains("$skiptoken=abc"));
    }

    #[test]
    fn parse_pairs_decodes_plus_and_percent() {
        let pairs = parse_query_pairs("a=hello+world&b=%2Fpath");
        let m: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(m["a"], "hello world");
        assert_eq!(m["b"], "/path");
    }

    #[test]
    fn path_segment_decodes_reserved_characters_and_keeps_plus() {
        // The two segments bifrost percent-encodes on a `$batch`
        // sub-request URL: the shared-mailbox owner and a base64-shaped
        // Graph message id.
        assert_eq!(
            decode_path_segment("shared%40contoso.com"),
            "shared@contoso.com"
        );
        assert_eq!(
            decode_path_segment("AAMkAGI2%2B9%2FxZ0%3D"),
            "AAMkAGI2+9/xZ0="
        );
        // A literal `+` is a `+` in a path segment, unlike in a query.
        assert_eq!(decode_path_segment("a+b"), "a+b");
        assert_eq!(decode_path_segment("a%20b"), "a b");
        // Lower-case hex, and malformed escapes passed through whole.
        assert_eq!(decode_path_segment("%2f%2F"), "//");
        assert_eq!(decode_path_segment("100%"), "100%");
        assert_eq!(decode_path_segment("%zz"), "%zz");
        assert_eq!(decode_path_segment("%2"), "%2");
    }
}
