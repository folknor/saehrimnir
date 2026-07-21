//! Hand-rolled XML helpers for the EWS SOAP surface.
//!
//! EWS request bodies are small and well-known, so - matching the
//! CalDAV module's philosophy (`caldav/xml.rs`) - we parse and emit by
//! hand rather than pulling a full XML stack. The request side only
//! needs to name the operation and pull a handful of ids / attributes;
//! the response side builds fixed-shape element trees.

/// XML text-escape for element content.
pub fn escape(s: &str) -> String {
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

/// True when `xml` contains an element whose local name is `name`
/// (ignoring any namespace prefix). Used to identify the SOAP
/// operation from the request body. The match is anchored: `name`
/// must be preceded by `<` or a prefix `:` and followed by
/// whitespace, `>`, or `/` so `Subscribe` does not match inside
/// `Unsubscribe` (preceded by `n`) or `SubscribeResponse`.
pub fn has_element(xml: &str, name: &str) -> bool {
    element_spans(xml, name).next().is_some()
}

/// Byte offsets of every `<...name...` element-open for the local
/// name `name`. Each yielded value is the index of the `<`.
fn element_spans<'a>(xml: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = xml.as_bytes();
    xml.match_indices(name).filter_map(move |(idx, _)| {
        // Char before the name: `<` (no prefix) or `:` (prefixed),
        // and if `:`, the char before the `<` chain is a `<` somewhere
        // - we only require `<` or `:` immediately preceding.
        if idx == 0 {
            return None;
        }
        let before = bytes[idx - 1];
        if before != b'<' && before != b':' {
            return None;
        }
        let after = bytes.get(idx + name.len()).copied();
        match after {
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') => {
                // Find the enclosing `<` (walk back over an optional
                // `prefix:`).
                let mut open = idx;
                while open > 0 && bytes[open - 1] != b'<' {
                    open -= 1;
                }
                if open > 0 { Some(open - 1) } else { None }
            }
            _ => None,
        }
    })
}

/// The value of attribute `attr` on the first element whose local name
/// is `tag`. e.g. `element_attr(body, "DistinguishedFolderId", "Id")`.
pub fn element_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    all_element_attr(xml, tag, attr).into_iter().next()
}

/// The value of attribute `attr` on every element whose local name is
/// `tag`, in document order. e.g. the `Id` on each `ItemId`.
pub fn all_element_attr(xml: &str, tag: &str, attr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for open in element_spans(xml, tag).collect::<Vec<_>>() {
        // Bound the search to this element's open tag (up to the next
        // `>`).
        let Some(gt) = xml[open..].find('>') else {
            continue;
        };
        let tag_span = &xml[open..open + gt];
        if let Some(v) = attr_in_span(tag_span, attr) {
            out.push(v);
        }
    }
    out
}

/// Pull `attr="value"` out of a single element-open span.
fn attr_in_span(span: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = span.find(&needle)? + needle.len();
    let end = span[start..].find('"')? + start;
    Some(span[start..end].to_string())
}

/// The text content of the first element whose local name is `tag`.
/// e.g. `element_text(body, "SubscriptionId")`.
pub fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = element_spans(xml, tag).next()?;
    let gt = xml[open..].find('>')? + open;
    // A self-closing element (`<tag/>`) has no text.
    if xml.as_bytes().get(gt - 1) == Some(&b'/') {
        return None;
    }
    let rest = &xml[gt + 1..];
    let close = rest.find("</")?;
    Some(rest[..close].trim().to_string())
}
