//! XML emission + light parsing for the CalDAV mock.
//!
//! Hand-rolled rather than pulling in `quick-xml` because the
//! responses we emit are well-known and small, the parsing we do
//! on inbound bodies is restricted to "did this property name
//! appear?", and avoiding a dependency keeps the determinism
//! contract obvious. Filled in as the verb handlers land.

/// Escape `s` for inclusion as XML element text. Replaces `&`,
/// `<`, `>`, `"`, and `'` with their entity references.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Walk the propfind body and return every requested property's
/// local name (namespace prefix stripped). PROPFIND handlers call
/// this once at entry and check membership on the resulting set;
/// parsing the body once per request is O(body) instead of
/// O(body * props) for the per-prop substring matcher.
///
/// Comments (`<!-- -->`), CDATA sections, processing instructions,
/// and `<!DOCTYPE>` declarations are skipped, which closes the
/// substring-match false-positive where a comment containing
/// `<some-prop>` would have been treated as a request for that
/// prop. Non-prop wrapper elements (`propfind`, `prop`) are
/// included in the set too; that's harmless because PROPFIND
/// handlers only look up specific known names.
pub(crate) fn requested_props(body: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<!--") {
            match body[i..].find("-->") {
                Some(end) => i += end + 3,
                None => return out,
            }
            continue;
        }
        if bytes[i..].starts_with(b"<![CDATA[") {
            match body[i..].find("]]>") {
                Some(end) => i += end + 3,
                None => return out,
            }
            continue;
        }
        if bytes[i..].starts_with(b"<?") {
            match body[i..].find("?>") {
                Some(end) => i += end + 2,
                None => return out,
            }
            continue;
        }
        if bytes[i..].starts_with(b"<!") {
            match body[i..].find('>') {
                Some(end) => i += end + 1,
                None => return out,
            }
            continue;
        }
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let after = &body[i + 1..];
        // Closing tags: skip.
        if after.starts_with('/') {
            i += 1;
            continue;
        }
        // Read the tag name (until the first whitespace, `/`, or `>`).
        let name_end = after
            .find([' ', '>', '/', '\t', '\n', '\r'])
            .unwrap_or(after.len());
        let raw_name = &after[..name_end];
        let local = match raw_name.find(':') {
            Some(j) => &raw_name[j + 1..],
            None => raw_name,
        };
        if !local.is_empty() {
            out.insert(local.to_string());
        }
        i += 1;
    }
    out
}

/// Returns true if the propfind body contains a request for the
/// named property, regardless of XML namespace prefix. Wrapper
/// around [`requested_props`] for callers that only need to
/// check one prop; PROPFIND handlers that check several should
/// build the set once and consult it directly.
pub(crate) fn body_requests_prop(body: &str, local_name: &str) -> bool {
    requested_props(body).contains(local_name)
}

/// Reduce a DAV `<href>` value to its absolute path.
///
/// RFC 4918 allows an href to be either a path or a full absolute URI,
/// and bifrost's CardDAV multiget sends the RESOLVED form
/// (`http://host:port/addressbooks/...`) because its listing resolves
/// every href against the response URL. A path-only `parse_path` maps
/// every such href to Unknown, which serves a 404 propstat per resource
/// and reads as "all contacts failed" with no error anywhere - the
/// silent wrong-shape failure mode this repo's lessons warn about.
pub(crate) fn href_path(href: &str) -> &str {
    let Some(scheme_end) = href.find("://") else {
        return href;
    };
    let after_authority = &href[scheme_end + 3..];
    match after_authority.find('/') {
        Some(slash) => &after_authority[slash..],
        None => "/",
    }
}

/// Extract every `<href>` element value from an XML body, regardless
/// of namespace prefix. Used by REPORT calendar-multiget to read
/// the list of requested resource URLs. Whitespace inside the
/// element is trimmed.
pub(crate) fn collect_hrefs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Walk the body looking for `<...href>...</...href>`. Tolerates
    // both `<href>` and `<D:href>` (any prefix). Keeps it simple:
    // we don't need full XML parsing for the multiget body shape
    // ratatoskr emits.
    let mut rest = body;
    loop {
        let Some(open_pos) = find_tag_open(rest, "href") else {
            return out;
        };
        let after_open = &rest[open_pos..];
        let Some(gt) = after_open.find('>') else {
            return out;
        };
        let value_start = open_pos + gt + 1;
        let value_slice = &rest[value_start..];
        let Some(close_pos) = find_tag_close(value_slice, "href") else {
            return out;
        };
        let value = value_slice[..close_pos].trim();
        if !value.is_empty() {
            out.push(value.to_string());
        }
        // Advance past this href's close tag.
        let after_close = close_pos + value_slice[close_pos..].find('>').unwrap_or(0) + 1;
        rest = &value_slice[after_close..];
    }
}

/// Extract the trimmed text inside the first `<...local_name>...
/// </...local_name>` element in `body`, ignoring namespace prefixes.
/// Returns `None` when the element is absent or self-closing.
/// Used by MKCALENDAR to read `displayname` / `calendar-color` /
/// `description` out of the request body. The result is *not*
/// XML-unescaped because the v0 fixture format doesn't use entity
/// references; clients that send `&amp;` in a displayname will see
/// the literal `&amp;` round-trip back through GET. Tracked as a
/// future limitation; trivial to lift if a fixture needs it.
pub(crate) fn text_value(body: &str, local_name: &str) -> Option<String> {
    let open = find_tag_open(body, local_name)?;
    let after_open = &body[open..];
    let gt = after_open.find('>')?;
    // Self-closing (`<displayname/>`) carries no text.
    if after_open.as_bytes()[gt - 1] == b'/' {
        return None;
    }
    let value_start = open + gt + 1;
    let value_slice = &body[value_start..];
    let close = find_tag_close(value_slice, local_name)?;
    let value = value_slice[..close].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Find an opening tag for `local_name`, allowing any namespace
/// prefix. Returns the byte offset of the opening `<` or `None`.
fn find_tag_open(haystack: &str, local_name: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find('<') {
        let abs = search_from + rel;
        let after = &haystack[abs + 1..];
        // Skip closing tags (`</...>`).
        if after.starts_with('/') {
            search_from = abs + 1;
            continue;
        }
        // Strip namespace prefix.
        let name_start = after.find(':').filter(|&i| {
            // The prefix is alphabetic and short; reject anything
            // containing whitespace or a slash before the colon.
            after[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
        let name_slice = match name_start {
            Some(i) => &after[i + 1..],
            None => after,
        };
        if name_slice.starts_with(local_name) {
            // Make sure the next byte is `>`, ` `, or `/` (i.e.
            // the tag boundary - not e.g. `hrefs`).
            let next_byte = name_slice.as_bytes().get(local_name.len());
            if matches!(next_byte, Some(b'>' | b' ' | b'/' | b'\t' | b'\n' | b'\r')) {
                return Some(abs);
            }
        }
        search_from = abs + 1;
    }
    None
}

/// Find the closing tag for `local_name` (any namespace prefix).
/// Returns the byte offset of the opening `<` of the close tag.
fn find_tag_close(haystack: &str, local_name: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find("</") {
        let abs = search_from + rel;
        let after = &haystack[abs + 2..];
        let name_slice = match after.find(':') {
            Some(i)
                if after[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
            {
                &after[i + 1..]
            }
            _ => after,
        };
        if name_slice.starts_with(local_name) {
            let next_byte = name_slice.as_bytes().get(local_name.len());
            if matches!(next_byte, Some(b'>' | b' ' | b'\t' | b'\n' | b'\r')) {
                return Some(abs);
            }
        }
        search_from = abs + 2;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_requests_prop_matches_self_closing_with_prefix() {
        let body = r#"<D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;
        assert!(body_requests_prop(body, "current-user-principal"));
        assert!(!body_requests_prop(body, "calendar-home-set"));
    }

    #[test]
    fn body_requests_prop_matches_bare_open_close() {
        let body = r#"<propfind xmlns="DAV:"><prop><current-user-principal></current-user-principal></prop></propfind>"#;
        assert!(body_requests_prop(body, "current-user-principal"));
    }

    #[test]
    fn href_path_strips_scheme_and_authority() {
        assert_eq!(
            href_path("http://127.0.0.1:12345/addressbooks/u/book/c.vcf"),
            "/addressbooks/u/book/c.vcf"
        );
        assert_eq!(
            href_path("https://dav.example.test/calendars/u/cal/"),
            "/calendars/u/cal/"
        );
        assert_eq!(href_path("/addressbooks/u/book/c.vcf"), "/addressbooks/u/book/c.vcf");
        assert_eq!(href_path("https://dav.example.test"), "/");
    }

    #[test]
    fn collect_hrefs_returns_each_href_in_order() {
        let body = r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
  <D:href>/calendars/u/cal/a.ics</D:href>
  <D:href>/calendars/u/cal/b.ics</D:href>
</C:calendar-multiget>"#;
        let hrefs = collect_hrefs(body);
        assert_eq!(
            hrefs,
            vec![
                "/calendars/u/cal/a.ics".to_string(),
                "/calendars/u/cal/b.ics".to_string(),
            ]
        );
    }

    #[test]
    fn escape_handles_ampersand_and_tags() {
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("<b>"), "&lt;b&gt;");
    }

    #[test]
    fn requested_props_skips_xml_comments() {
        // Substring-only matchers used to false-positive when a
        // prop name appeared inside a comment.
        let body = r#"<D:propfind xmlns:D="DAV:">
            <!-- earlier draft asked for <D:current-user-principal/> -->
            <D:prop><D:displayname/></D:prop>
        </D:propfind>"#;
        let props = requested_props(body);
        assert!(props.contains("displayname"));
        assert!(
            !props.contains("current-user-principal"),
            "comment-resident prop name leaked into the requested set: {props:?}"
        );
    }

    #[test]
    fn requested_props_skips_processing_instructions_and_doctype() {
        let body = r#"<?xml version="1.0"?>
            <!DOCTYPE propfind PUBLIC "ignored" "ignored">
            <D:propfind xmlns:D="DAV:"><D:prop><D:displayname/></D:prop></D:propfind>"#;
        let props = requested_props(body);
        assert!(props.contains("displayname"));
    }

    #[test]
    fn requested_props_handles_namespace_prefixes_and_no_prefix() {
        let body = r#"<propfind xmlns="DAV:"><prop>
            <displayname/>
            <D:getctag xmlns:D="DAV:"/>
        </prop></propfind>"#;
        let props = requested_props(body);
        assert!(props.contains("displayname"));
        assert!(props.contains("getctag"));
    }
}
