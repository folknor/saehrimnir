//! Minimal vCard projection for the CardDAV mock.
//!
//! Emits a vCard 3.0 body from a fixture [`Contact`] on the read path
//! (`GET` / `REPORT`), and parses the subset of a written vCard body
//! bifrost-carddav sends back on `PUT` so a phone / org / notes write
//! round-trips on the next read.
//!
//! bifrost's own vCard projection (`crates/carddav/src/vcard.rs`)
//! parses `FN` / `N` / `EMAIL` / `TEL` / `ORG` / `TITLE` / `NOTE`, so
//! the read-path serializer emits exactly those. The write-path parser
//! reads the same set back.
//!
//! Deliberate malformed affordance: a contact flagged
//! `malformed_vcard` serializes to a body carrying an unterminated
//! quoted parameter, which bifrost's tokenizer rejects (missing closing
//! quote). bifrost then records the resource id in `Page::failed_ids`
//! rather than treating the absence as a deletion.

use crate::fixture::{Contact, ContactEmail, ContactPhone};

/// Serialize a fixture [`Contact`] into a vCard 3.0 body (CRLF line
/// endings, RFC 6350 escaping on text values). When
/// `contact.malformed_vcard` is set, the body is deliberately
/// unparseable so bifrost routes the resource to `failed_ids`.
pub(crate) fn contact_to_vcard(contact: &Contact) -> String {
    if contact.malformed_vcard {
        return malformed_vcard(contact);
    }
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCARD");
    push_line(&mut out, "VERSION:3.0");
    push_line(&mut out, &format!("UID:{}", escape_text(&contact.id)));
    let display = contact.display_name.as_deref().unwrap_or("");
    push_line(&mut out, &format!("FN:{}", escape_text(display)));
    // N is mandatory in vCard 3.0; put the whole display name in the
    // family-name slot so a 3.0-strict parser accepts the card.
    push_line(&mut out, &format!("N:{};;;;", escape_text(display)));
    for email in &contact.emails {
        push_line(&mut out, &email_line(email));
    }
    for phone in &contact.phones {
        push_line(&mut out, &phone_line(phone));
    }
    if let Some(company) = &contact.company {
        push_line(&mut out, &format!("ORG:{}", escape_text(company)));
    }
    if let Some(title) = &contact.job_title {
        push_line(&mut out, &format!("TITLE:{}", escape_text(title)));
    }
    if let Some(notes) = &contact.notes {
        push_line(&mut out, &format!("NOTE:{}", escape_text(notes)));
    }
    push_line(&mut out, "END:VCARD");
    out
}

/// A body bifrost cannot tokenize: the EMAIL parameter opens a quoted
/// value that is never closed, which bifrost's `split_content_line`
/// rejects with "missing a closing quote".
fn malformed_vcard(contact: &Contact) -> String {
    let mut out = String::new();
    push_line(&mut out, "BEGIN:VCARD");
    push_line(&mut out, "VERSION:3.0");
    push_line(&mut out, &format!("UID:{}", escape_text(&contact.id)));
    let display = contact.display_name.as_deref().unwrap_or("");
    push_line(&mut out, &format!("FN:{}", escape_text(display)));
    let addr = contact
        .emails
        .first()
        .map(|e| e.address.as_str())
        .unwrap_or("nobody@example.test");
    push_line(&mut out, &format!("EMAIL;TYPE=\"unterminated:{addr}"));
    push_line(&mut out, "END:VCARD");
    out
}

fn email_line(email: &ContactEmail) -> String {
    // The fixture email carries no kind; emit a bare EMAIL. bifrost
    // reads the address and a `None` kind, which is what a bare EMAIL
    // conveys.
    format!("EMAIL:{}", escape_text(&email.address))
}

fn phone_line(phone: &ContactPhone) -> String {
    match &phone.kind {
        Some(kind) if !kind.is_empty() => {
            format!(
                "TEL;TYPE={}:{}",
                escape_param(kind),
                escape_text(&phone.number)
            )
        }
        _ => format!("TEL:{}", escape_text(&phone.number)),
    }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push_str("\r\n");
}

/// Escape an RFC 6350 text value: backslash, newline, `;` and `,`.
fn escape_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

/// Escape a parameter value; quote it when it carries a delimiter.
fn escape_param(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "");
    if escaped.contains([';', ',', ':']) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

/// The subset of vCard fields the CardDAV mock stores from a written
/// body.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedVCard {
    pub(crate) uid: Option<String>,
    pub(crate) display_name: Option<String>,
    pub(crate) emails: Vec<ContactEmail>,
    pub(crate) phones: Vec<ContactPhone>,
    pub(crate) company: Option<String>,
    pub(crate) job_title: Option<String>,
    pub(crate) notes: Option<String>,
}

/// Parse a written vCard body into the stored subset. Physical-line
/// folding (leading SPACE/TAB continuations) is unfolded first. Only
/// the fields the mock persists are extracted; everything else is
/// ignored.
pub(crate) fn parse_vcard(body: &str) -> ParsedVCard {
    let mut parsed = ParsedVCard::default();
    for line in unfold(body) {
        let (name_and_params, value) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let mut parts = name_and_params.split(';');
        let raw_name = parts.next().unwrap_or("");
        // Strip an Apple group prefix (`item1.EMAIL` -> `EMAIL`).
        let name = raw_name
            .rsplit_once('.')
            .map(|(_, n)| n)
            .unwrap_or(raw_name)
            .to_ascii_uppercase();
        let params: Vec<&str> = parts.collect();
        let text = unescape_text(value);
        match name.as_str() {
            "UID" if !text.is_empty() => parsed.uid = Some(text),
            "FN" if !text.is_empty() => parsed.display_name = Some(text),
            "N" if parsed.display_name.is_none() && !text.is_empty() => {
                // N family;given;... -> use the family slot as a
                // fallback display name when FN is absent.
                let family = value.split(';').next().unwrap_or("");
                if !family.is_empty() {
                    parsed.display_name = Some(unescape_text(family));
                }
            }
            "EMAIL" if !text.is_empty() => parsed.emails.push(ContactEmail {
                address: text,
                name: None,
            }),
            "TEL" if !text.is_empty() => parsed.phones.push(ContactPhone {
                number: text,
                kind: type_param(&params),
            }),
            "ORG" if !text.is_empty() => {
                // ORG is `;`-structured; the first component is the
                // company name.
                let company = value.split(';').next().unwrap_or("");
                if !company.is_empty() {
                    parsed.company = Some(unescape_text(company));
                }
            }
            "TITLE" if !text.is_empty() => parsed.job_title = Some(text),
            "NOTE" if !text.is_empty() => parsed.notes = Some(text),
            _ => {}
        }
    }
    parsed
}

/// Unfold physical lines: a line starting with SPACE or TAB is a
/// continuation of the previous logical line (RFC 6350 §3.2), with one
/// leading WSP removed.
fn unfold(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = out.last_mut()
        {
            last.push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// First `TYPE=` value among the parameters, lowercased. A bare param
/// (vCard 3.0 shorthand, `TEL;WORK:`) is read as a type value too.
fn type_param(params: &[&str]) -> Option<String> {
    for p in params {
        if let Some(v) = p.strip_prefix("TYPE=").or_else(|| p.strip_prefix("type=")) {
            let v = v.trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_ascii_lowercase());
            }
        } else if !p.contains('=') && !p.is_empty() {
            return Some(p.trim_matches('"').to_ascii_lowercase());
        }
    }
    None
}

fn unescape_text(value: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            match ch {
                'n' | 'N' => out.push('\n'),
                '\\' | ';' | ',' => out.push(ch),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: &str) -> Contact {
        Contact {
            id: id.to_string(),
            account_id: "acct".to_string(),
            folder_id: "book".to_string(),
            display_name: Some("Ada Lovelace".to_string()),
            emails: vec![ContactEmail {
                address: "ada@example.test".to_string(),
                name: None,
            }],
            phones: vec![ContactPhone {
                number: "+123".to_string(),
                kind: Some("mobile".to_string()),
            }],
            company: Some("Analytical Engines".to_string()),
            job_title: Some("Programmer".to_string()),
            department: None,
            notes: Some("first algorithm".to_string()),
            groups: Vec::new(),
            malformed_vcard: false,
        }
    }

    #[test]
    fn serialize_then_parse_round_trips_enriched_fields() {
        let c = contact("ada");
        let body = contact_to_vcard(&c);
        assert!(body.contains("FN:Ada Lovelace"));
        assert!(body.contains("EMAIL:ada@example.test"));
        assert!(body.contains("TEL;TYPE=mobile:+123"));
        assert!(body.contains("ORG:Analytical Engines"));
        assert!(body.contains("TITLE:Programmer"));
        assert!(body.contains("NOTE:first algorithm"));

        let parsed = parse_vcard(&body);
        assert_eq!(parsed.uid.as_deref(), Some("ada"));
        assert_eq!(parsed.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(parsed.emails[0].address, "ada@example.test");
        assert_eq!(parsed.phones[0].number, "+123");
        assert_eq!(parsed.phones[0].kind.as_deref(), Some("mobile"));
        assert_eq!(parsed.company.as_deref(), Some("Analytical Engines"));
        assert_eq!(parsed.job_title.as_deref(), Some("Programmer"));
        assert_eq!(parsed.notes.as_deref(), Some("first algorithm"));
    }

    #[test]
    fn malformed_body_has_unterminated_quote() {
        let mut c = contact("bad");
        c.malformed_vcard = true;
        let body = contact_to_vcard(&c);
        // Opening quote with no closing quote before the line ends.
        assert!(body.contains("EMAIL;TYPE=\"unterminated:"));
        assert_eq!(body.matches('"').count(), 1);
    }
}
