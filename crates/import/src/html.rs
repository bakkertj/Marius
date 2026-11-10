//! HTML → Markdown (IM-008).

use crate::{yaml_quote, ImportError, ImportedNote};

/// Decode an HTML file's bytes honestly. Outlook's "save as .htm" writes
/// the system codepage (Windows-1252) or UTF-16 — treating that as UTF-8
/// turns every non-breaking space into U+FFFD, and a user saw their
/// email's whole header block as "triangles with question marks".
///
/// Order: a BOM wins; then the page's own `<meta charset>`; then UTF-8
/// if the bytes actually are; else Windows-1252, the web's historical
/// default and a superset of Latin-1.
pub fn decode(bytes: &[u8]) -> String {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding
            .decode_without_bom_handling(&bytes[bom_len..])
            .0
            .into_owned();
    }
    if let Some(encoding) = declared_charset(bytes) {
        return encoding.decode_without_bom_handling(bytes).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
    }
}

/// The charset the page declares about itself, scanned ASCII-wise in the
/// head: `<meta charset="x">` or the older
/// `<meta http-equiv=... content="text/html; charset=x">`.
fn declared_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head: String = bytes
        .iter()
        .take(4096)
        .map(|&b| if b.is_ascii() { b as char } else { ' ' })
        .collect::<String>()
        .to_lowercase();
    let at = head.find("charset")?;
    let rest = head[at + "charset".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| c == '"' || c == '\'' || c == ';' || c == '>' || c.is_whitespace())
        .unwrap_or(rest.len());
    encoding_rs::Encoding::for_label(rest[..end].trim().as_bytes())
}

/// Convert an HTML document into a note. `source` names where it came
/// from (a path or URL) and lands in frontmatter; the title is the
//  document's own <title> when it has one.
pub fn import(
    html: &str,
    fallback_title: &str,
    source: Option<&str>,
) -> Result<ImportedNote, ImportError> {
    let markdown = crate::html_to_markdown(html).map_err(|e| ImportError::Html(e.to_string()))?;
    let title = extract_title(html).unwrap_or_else(|| fallback_title.to_owned());

    let mut body = String::from("---\n");
    body.push_str(&format!("title: {}\n", yaml_quote(&title)));
    if let Some(source) = source {
        body.push_str(&format!("source: {}\n", yaml_quote(source)));
    }
    body.push_str("imported: html\n---\n\n");
    body.push_str(markdown.trim());
    body.push('\n');

    Ok(ImportedNote {
        file_name: format!("{title}.md"),
        body,
        attachments: Vec::new(),
    })
}

/// The document's own <title>, decoded just enough for real pages.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    let start = lower[open..].find('>')? + open + 1;
    let end = lower[start..].find("</title>")? + start;
    let raw = html[start..end].trim();
    if raw.is_empty() {
        return None;
    }
    let decoded = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace(['\n', '\r'], " ");
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_becomes_a_frontmattered_markdown_note() {
        let html = r#"<html><head><title>Lighthouse &amp; Harbour</title></head>
            <body><h1>Pharos</h1><p>It stood for <strong>sixteen</strong> centuries.</p>
            <ul><li>limestone</li><li>lead</li></ul></body></html>"#;
        let note = import(html, "fallback", Some("https://example.org/pharos")).unwrap();
        assert_eq!(note.file_name, "Lighthouse & Harbour.md");
        assert!(note.body.starts_with("---\n"));
        assert!(note.body.contains(r#"title: "Lighthouse & Harbour""#));
        assert!(note
            .body
            .contains(r#"source: "https://example.org/pharos""#));
        assert!(note.body.contains("# Pharos"));
        assert!(note.body.contains("**sixteen**"));
        assert!(note.body.contains("limestone"));
    }

    #[test]
    fn a_titleless_page_uses_the_fallback() {
        let note = import("<p>just text</p>", "clipping", None).unwrap();
        assert_eq!(note.file_name, "clipping.md");
        assert!(note.body.contains("just text"));
    }

    #[test]
    fn outlook_cp1252_headers_decode_without_replacement_characters() {
        // "From:" + cp1252 0xA0 nbsp + "R" + 0xE9 "é" — the exact bytes
        // Outlook's save-as-htm writes for its header block.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            b"<html><head><meta http-equiv=Content-Type \
              content=\"text/html; charset=windows-1252\"></head><body>From:",
        );
        bytes.push(0xA0);
        bytes.extend_from_slice(b"R");
        bytes.push(0xE9);
        bytes.extend_from_slice(b"my</body></html>");
        let text = decode(&bytes);
        assert!(!text.contains('\u{FFFD}'), "{text:?}");
        assert!(text.contains("R\u{e9}my"), "{text:?}");
    }

    #[test]
    fn utf16_with_bom_and_plain_utf8_both_decode() {
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "<p>Phare</p>".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(decode(&utf16).contains("Phare"));
        assert_eq!(decode("d\u{e9}j\u{e0}".as_bytes()), "d\u{e9}j\u{e0}");
    }

    #[test]
    fn undeclared_non_utf8_falls_back_to_windows_1252() {
        let bytes = b"<body>caf\xe9</body>";
        assert!(decode(bytes).contains("caf\u{e9}"));
    }
}
