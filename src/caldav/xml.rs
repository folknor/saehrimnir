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

/// Returns true if the propfind body contains a request for the
/// named property, regardless of XML namespace prefix. v0's
/// matcher is a simple substring search on the local name surrounded
/// by element-shape characters (`<` / `>` / whitespace / `:`); good
/// enough since the bodies are small and the property names are
/// distinctive.
pub(crate) fn body_requests_prop(body: &str, local_name: &str) -> bool {
    // Match on `:<local-name>/`, `:<local-name>>`, `<local-name>/`,
    // or `<local-name>>`. Covers `<D:current-user-principal/>`
    // (self-closing with namespace prefix), `<current-user-principal/>`
    // (default namespace), and the open/close form
    // `<C:calendar-home-set></C:calendar-home-set>`.
    let needle_prefixed_close = format!(":{local_name}/");
    let needle_prefixed_open = format!(":{local_name}>");
    let needle_bare_close = format!("<{local_name}/");
    let needle_bare_open = format!("<{local_name}>");
    body.contains(&needle_prefixed_close)
        || body.contains(&needle_prefixed_open)
        || body.contains(&needle_bare_close)
        || body.contains(&needle_bare_open)
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
            after[..i].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
            Some(i) if after[..i].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') => {
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
}
